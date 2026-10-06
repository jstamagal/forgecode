use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use anyhow::Context;
use forge_app::domain::{
    McpConfig, McpServerConfig, McpServers, ServerName, ToolCallFull, ToolCatalog, ToolDefinition,
    ToolName, ToolOutput, ToolValue,
};
use forge_app::{
    EnvironmentInfra, KVStore, McpClientInfra, McpConfigManager, McpServerInfra, McpService,
};
use strum::IntoEnumIterator;
use tokio::sync::{Mutex, RwLock};

use crate::mcp::tool::McpExecutor;

/// Names MCP tools may not take: native tools and `tool_discovery`.
fn reserved_tool_names() -> BTreeSet<String> {
    ToolCatalog::iter()
        .map(|tool| tool.definition().name.to_string())
        .collect()
}

/// Redacts configured secrets from every text value of `output`.
fn redact_output(output: ToolOutput, secrets: &[String]) -> ToolOutput {
    if secrets.is_empty() {
        return output;
    }
    let values = output
        .values
        .into_iter()
        .map(|value| match value {
            ToolValue::Text(text) => ToolValue::Text(forge_app::domain::redact(&text, secrets)),
            other => other,
        })
        .collect();
    ToolOutput { values, ..output }
}

#[derive(Clone)]
pub struct ForgeMcpService<M, I, C> {
    tools: Arc<RwLock<HashMap<ToolName, ToolHolder<McpExecutor<C>>>>>,
    /// Connected clients by server name (M3 controls address a server).
    clients: Arc<RwLock<HashMap<String, Arc<C>>>>,
    failed_servers: Arc<RwLock<HashMap<ServerName, String>>>,
    /// M5: configured secret values per server, redacted from results and
    /// errors.
    secrets: Arc<RwLock<HashMap<String, Vec<String>>>>,
    previous_config_hash: Arc<Mutex<u64>>,
    init_lock: Arc<Mutex<()>>,
    manager: Arc<M>,
    infra: Arc<I>,
}

#[derive(Clone)]
struct ToolHolder<T> {
    definition: ToolDefinition,
    executable: T,
    server_name: String,
}

impl<M, I, C> ForgeMcpService<M, I, C>
where
    M: McpConfigManager,
    I: McpServerInfra + KVStore + EnvironmentInfra,
    C: McpClientInfra + Clone,
    C: From<<I as McpServerInfra>::Client>,
{
    pub fn new(manager: Arc<M>, infra: Arc<I>) -> Self {
        Self {
            tools: Default::default(),
            clients: Default::default(),
            failed_servers: Default::default(),
            secrets: Default::default(),
            previous_config_hash: Arc::new(Mutex::new(Default::default())),
            init_lock: Arc::new(Mutex::new(())),
            manager,
            infra,
        }
    }

    async fn is_config_modified(&self, config: &McpConfig) -> bool {
        *self.previous_config_hash.lock().await != config.cache_key()
    }

    /// M1: publishes every connected server's tools as `{server}__{tool}`,
    /// trimmed to 64 characters, with collisions disambiguated (and logged)
    /// and native tool names withheld.
    async fn insert_clients(
        &self,
        connected: Vec<(ServerName, Arc<C>, Vec<ToolDefinition>)>,
    ) -> anyhow::Result<()> {
        let listing: BTreeMap<String, Vec<String>> = connected
            .iter()
            .map(|(server, _, tools)| {
                (
                    server.to_string(),
                    tools.iter().map(|t| t.name.to_string()).collect(),
                )
            })
            .collect();
        let (names, warnings) =
            forge_app::domain::assign_public_names(&listing, &reserved_tool_names());
        for warning in warnings {
            tracing::warn!("{warning}");
        }
        let mut tool_map = self.tools.write().await;
        let mut clients = self.clients.write().await;
        for (server_name, client, tools) in connected {
            clients.insert(server_name.to_string(), client.clone());
            for mut tool in tools {
                let key = (server_name.to_string(), tool.name.to_string());
                let Some(public) = names.get(&key) else {
                    continue;
                };
                let executable = McpExecutor::new(tool.name.clone(), client.clone())?;
                tool.name = ToolName::new(public);
                tool_map.insert(
                    tool.name.clone(),
                    ToolHolder {
                        definition: tool,
                        executable,
                        server_name: server_name.to_string(),
                    },
                );
            }
        }
        Ok(())
    }

    async fn connect(
        &self,
        server_name: &ServerName,
        config: McpServerConfig,
    ) -> anyhow::Result<(Arc<C>, Vec<ToolDefinition>)> {
        let env_vars = self.infra.get_env_vars();
        let environment = self.infra.get_environment();
        let client = self.infra.connect(config, &env_vars, &environment).await?;
        let client = Arc::new(C::from(client));
        let tools = client
            .list()
            .await
            .with_context(|| format!("Failed to list tools of MCP server {server_name}"))?;
        Ok((client, tools))
    }

    async fn ensure_mcp_initialized(&self) -> anyhow::Result<()> {
        let raw_mcp = self.manager.read_mcp_config(None).await?;

        // Fast path: if config is unchanged, skip reinitialization without
        // acquiring the lock
        if !self.is_config_modified(&raw_mcp).await {
            return Ok(());
        }

        // Serialise concurrent initialisations so only one caller runs
        // update_mcp at a time
        let _guard = self.init_lock.lock().await;

        // Double-check under the lock: a concurrent caller may have already
        // updated
        if !self.is_config_modified(&raw_mcp).await {
            return Ok(());
        }

        // Apply the trust gate. The prompt was already shown at startup via
        // init_mcp, so on first tool use the user's decision is re-applied by
        // calling filter_trusted again.
        let trusted_mcp = self.manager.filter_trusted(raw_mcp).await?;

        self.update_mcp(trusted_mcp).await
    }

    async fn update_mcp(&self, mcp: McpConfig) -> Result<(), anyhow::Error> {
        // Use the raw config hash (pre-trust-gate) so that is_config_modified
        // always compares against the original file hash, preventing
        // infinite re-prompt loops when some servers are rejected.
        let new_hash = mcp.cache_key();
        self.clear_tools().await;

        // Clear failed servers map before attempting new connections
        self.failed_servers.write().await.clear();
        *self.secrets.write().await = mcp
            .mcp_servers
            .iter()
            .map(|(name, server)| (name.to_string(), forge_app::domain::secret_values(server)))
            .collect();

        let connections: Vec<_> = mcp
            .mcp_servers
            .into_iter()
            .filter(|v| !v.1.is_disabled())
            .map(|(name, server)| async move {
                let conn = self
                    .connect(&name, server)
                    .await
                    .context(format!("Failed to initiate MCP server: {name}"));

                (name, conn)
            })
            .collect();

        let results = futures::future::join_all(connections).await;

        let mut connected = Vec::new();
        for (server_name, result) in results {
            match result {
                Ok((client, tools)) => connected.push((server_name, client, tools)),
                Err(error) => {
                    // Format error with full chain for detailed diagnostics,
                    // with configured secrets redacted (M5).
                    let secrets = self
                        .secrets
                        .read()
                        .await
                        .get(&server_name.to_string())
                        .cloned()
                        .unwrap_or_default();
                    let error_string = forge_app::domain::redact(&format!("{error:?}"), &secrets);
                    self.failed_servers
                        .write()
                        .await
                        .insert(server_name.clone(), error_string);
                }
            }
        }
        self.insert_clients(connected).await?;

        // Write the hash only after join_all finishes so that any waiter on
        // init_lock re-checks is_config_modified only once self.tools is fully
        // populated, preventing "Tool not found" races.
        *self.previous_config_hash.lock().await = new_hash;

        Ok(())
    }

    async fn list(&self) -> anyhow::Result<McpServers> {
        self.ensure_mcp_initialized().await?;
        self.refresh_changed_servers().await?;

        let tools = self.tools.read().await;
        let mut grouped_tools = std::collections::HashMap::new();

        for tool in tools.values() {
            grouped_tools
                .entry(ServerName::from(tool.server_name.clone()))
                .or_insert_with(Vec::new)
                .push(tool.definition.clone());
        }

        let failures = self.failed_servers.read().await.clone();

        Ok(McpServers::new(grouped_tools, failures))
    }

    /// M2: re-lists the tools of every server that sent
    /// `notifications/tools/list_changed`, and republishes the names of all
    /// connected servers (a new remote tool can collide with another
    /// server's). A failed refresh keeps the server marked changed.
    async fn refresh_changed_servers(&self) -> anyhow::Result<()> {
        let clients: Vec<(String, Arc<C>)> = self
            .clients
            .read()
            .await
            .iter()
            .map(|(name, client)| (name.clone(), client.clone()))
            .collect();
        let changed: Vec<&(String, Arc<C>)> = clients
            .iter()
            .filter(|(_, client)| client.take_tools_changed())
            .collect();
        if changed.is_empty() {
            return Ok(());
        }
        let mut connected = Vec::new();
        for (name, client) in &clients {
            match client.list().await {
                Ok(tools) => {
                    connected.push((ServerName::from(name.clone()), client.clone(), tools))
                }
                Err(error) => {
                    tracing::warn!(server = %name, %error, "MCP tools/list refresh failed")
                }
            }
        }
        self.tools.write().await.clear();
        self.insert_clients(connected).await?;
        // Cached listings are stale now.
        self.infra.cache_clear().await
    }
    async fn clear_tools(&self) {
        self.tools.write().await.clear();
        self.clients.write().await.clear();
    }

    async fn call(&self, call: ToolCallFull) -> anyhow::Result<ToolOutput> {
        // Ensure MCP connections are initialized before calling tools
        self.ensure_mcp_initialized().await?;

        let tools = self.tools.read().await;

        // M1: the public name is `{server}__{tool}`; the Claude Code form
        // `mcp__{server}__{tool}` and the old `mcp_{server}_tool_{tool}` form
        // are accepted on incoming calls.
        let name = call.name.as_str();
        let legacy = name
            .strip_prefix("mcp_")
            .and_then(|rest| rest.split_once("_tool_"))
            .map(|(server, tool)| forge_app::domain::public_tool_name(server, tool));
        let tool = tools
            .get(&call.name)
            .or_else(|| {
                name.strip_prefix("mcp__")
                    .and_then(|n| tools.get(&ToolName::new(n)))
            })
            .or_else(|| legacy.and_then(|n| tools.get(&ToolName::new(n))))
            .with_context(|| format!("MCP tool {name} is no longer available"))?;

        let secrets = self
            .secrets
            .read()
            .await
            .get(&tool.server_name)
            .cloned()
            .unwrap_or_default();
        match tool.executable.call_tool(call.arguments.parse()?).await {
            Ok(output) => Ok(redact_output(output, &secrets)),
            Err(error) => Err(anyhow::anyhow!(forge_app::domain::redact(
                &format!("{error:#}"),
                &secrets
            ))),
        }
    }

    /// Refresh the MCP cache by clearing cached data.
    /// Does NOT eagerly connect to servers - connections happen lazily
    /// when list() or call() is invoked, avoiding interactive OAuth during
    /// reload.
    async fn refresh_cache(&self) -> anyhow::Result<()> {
        // Hold init_lock so we don't race with an in-flight update_mcp: without
        // this, clear_tools could run while connections are still being
        // established, leaving waiters released into an empty tool map.
        let _guard = self.init_lock.lock().await;
        // Clear the infra cache and reset config hash to force re-init on next
        // access
        self.infra.cache_clear().await?;
        *self.previous_config_hash.lock().await = Default::default();
        self.clear_tools().await;
        self.failed_servers.write().await.clear();
        Ok(())
    }
}

#[async_trait::async_trait]
impl<M: McpConfigManager, I: McpServerInfra + KVStore + EnvironmentInfra, C> McpService
    for ForgeMcpService<M, I, C>
where
    C: McpClientInfra + Clone,
    C: From<<I as McpServerInfra>::Client>,
{
    async fn get_mcp_servers(&self) -> anyhow::Result<McpServers> {
        // Apply the trust gate before computing the cache key so that rejected
        // servers are excluded. Using the raw config hash would allow a stale
        // KV cache entry (populated before a rejection) to be returned,
        // bypassing filter_trusted entirely and leaking rejected tools
        // into requests.
        let raw_mcp = self.manager.read_mcp_config(None).await?;
        let trusted_mcp = self.manager.filter_trusted(raw_mcp).await?;
        let config_hash = trusted_mcp.cache_key();

        // A server that announced a changed tool list invalidates the cache.
        self.refresh_changed_servers().await?;

        // Check if cache is valid (exists and not expired)
        if let Some(cache) = self.infra.cache_get::<_, McpServers>(&config_hash).await? {
            return Ok(cache.clone());
        }

        let servers = self.list().await?;
        self.infra.cache_set(&config_hash, &servers).await?;
        Ok(servers)
    }

    async fn execute_mcp(&self, call: ToolCallFull) -> anyhow::Result<ToolOutput> {
        self.call(call).await
    }

    async fn mcp_control(
        &self,
        server: &str,
        request: forge_app::McpControl,
    ) -> anyhow::Result<serde_json::Value> {
        self.ensure_mcp_initialized().await?;
        let client = self
            .clients
            .read()
            .await
            .get(server)
            .cloned()
            .with_context(|| format!("MCP server {server:?} is not connected"))?;
        let secrets = self
            .secrets
            .read()
            .await
            .get(server)
            .cloned()
            .unwrap_or_default();
        match client.control(request).await {
            Ok(value) => {
                let text = forge_app::domain::redact(&value.to_string(), &secrets);
                Ok(serde_json::from_str(&text).unwrap_or(serde_json::Value::String(text)))
            }
            Err(error) => Err(anyhow::anyhow!(forge_app::domain::redact(
                &format!("{error:#}"),
                &secrets
            ))),
        }
    }

    async fn reload_mcp(&self) -> anyhow::Result<()> {
        self.refresh_cache().await
    }

    async fn init_mcp(&self) -> anyhow::Result<()> {
        // Run the trust gate prompt at startup so the user's decision is
        // captured before any tool use. The result is intentionally
        // discarded — servers are NOT connected here. Connections
        // remain lazy and happen on first tool use
        // via ensure_mcp_initialized.
        let raw_mcp = self.manager.read_mcp_config(None).await?;
        let _ = self.manager.filter_trusted(raw_mcp).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use fake::{Fake, Faker};
    use forge_app::domain::{
        ConfigOperation, Environment, McpConfig, McpServerConfig, Scope, ServerName, ToolCallFull,
        ToolDefinition, ToolName, ToolOutput,
    };
    use forge_app::{
        EnvironmentInfra, KVStore, McpClientInfra, McpConfigManager, McpServerInfra, McpService,
    };
    use forge_config::ForgeConfig;
    use pretty_assertions::assert_eq;
    use serde::de::DeserializeOwned;

    use super::ForgeMcpService;

    // ── Mock MCP client ──────────────────────────────────────────────────────

    /// Mock MCP server; `generation` > 0 adds a second tool, and `changed`
    /// is the pending list_changed flag.
    #[derive(Clone, Default)]
    struct MockMcpClient {
        generation: Arc<std::sync::atomic::AtomicUsize>,
        changed: Arc<std::sync::atomic::AtomicBool>,
    }

    #[async_trait::async_trait]
    impl McpClientInfra for MockMcpClient {
        async fn list(&self) -> anyhow::Result<Vec<ToolDefinition>> {
            Ok(
                match self.generation.load(std::sync::atomic::Ordering::SeqCst) {
                    0 => vec![ToolDefinition::new("test_tool")],
                    _ => vec![
                        ToolDefinition::new("test_tool"),
                        ToolDefinition::new("new_tool"),
                    ],
                },
            )
        }

        fn take_tools_changed(&self) -> bool {
            self.changed
                .swap(false, std::sync::atomic::Ordering::SeqCst)
        }

        async fn call(
            &self,
            _tool_name: &ToolName,
            _input: serde_json::Value,
        ) -> anyhow::Result<ToolOutput> {
            Ok(ToolOutput::text("mock result"))
        }

        async fn control(
            &self,
            request: forge_app::McpControl,
        ) -> anyhow::Result<serde_json::Value> {
            Ok(match request {
                forge_app::McpControl::ListResources => {
                    serde_json::json!([{"uri": "file:///notes.md", "name": "notes"}])
                }
                forge_app::McpControl::ReadResource { uri } => {
                    serde_json::json!({"contents": [{"uri": uri, "text": "token=s3cr3t-v4lue"}]})
                }
                other => serde_json::json!({"request": format!("{other:?}")}),
            })
        }
    }

    // ── Mock config manager ──────────────────────────────────────────────────

    struct MockMcpManager;

    #[async_trait::async_trait]
    impl McpConfigManager for MockMcpManager {
        async fn read_mcp_config(&self, _scope: Option<&Scope>) -> anyhow::Result<McpConfig> {
            let mut servers = BTreeMap::new();
            servers.insert(
                ServerName::from("test-server".to_string()),
                McpServerConfig::new_stdio(
                    "echo",
                    vec![],
                    Some(BTreeMap::from([(
                        "API_TOKEN".to_string(),
                        "s3cr3t-v4lue".to_string(),
                    )])),
                ),
            );
            Ok(McpConfig { mcp_servers: servers })
        }

        async fn write_mcp_config(
            &self,
            _config: &McpConfig,
            _scope: &Scope,
        ) -> anyhow::Result<()> {
            Ok(())
        }

        async fn filter_trusted(&self, raw: McpConfig) -> anyhow::Result<McpConfig> {
            // In tests all configs are implicitly trusted.
            Ok(raw)
        }
    }

    // ── Mock infrastructure ──────────────────────────────────────────────────

    #[derive(Clone, Default)]
    struct MockInfra {
        client: MockMcpClient,
    }

    #[async_trait::async_trait]
    impl McpServerInfra for MockInfra {
        type Client = MockMcpClient;

        async fn connect(
            &self,
            _config: McpServerConfig,
            _env_vars: &BTreeMap<String, String>,
            _environment: &Environment,
        ) -> anyhow::Result<MockMcpClient> {
            Ok(self.client.clone())
        }
    }

    #[async_trait::async_trait]
    impl KVStore for MockInfra {
        async fn cache_get<K, V>(&self, _key: &K) -> anyhow::Result<Option<V>>
        where
            K: std::hash::Hash + Sync,
            V: serde::Serialize + DeserializeOwned + Send,
        {
            Ok(None)
        }

        async fn cache_set<K, V>(&self, _key: &K, _value: &V) -> anyhow::Result<()>
        where
            K: std::hash::Hash + Sync,
            V: serde::Serialize + Sync,
        {
            Ok(())
        }

        async fn cache_clear(&self) -> anyhow::Result<()> {
            Ok(())
        }
    }

    impl EnvironmentInfra for MockInfra {
        type Config = ForgeConfig;

        fn get_env_var(&self, _key: &str) -> Option<String> {
            None
        }

        fn get_env_vars(&self) -> BTreeMap<String, String> {
            BTreeMap::new()
        }

        fn get_environment(&self) -> Environment {
            Faker.fake()
        }

        fn get_config(&self) -> anyhow::Result<ForgeConfig> {
            Ok(ForgeConfig::default())
        }

        async fn update_environment(&self, _ops: Vec<ConfigOperation>) -> anyhow::Result<()> {
            Ok(())
        }
    }

    // ── Fixture ──────────────────────────────────────────────────────────────

    fn fixture() -> ForgeMcpService<MockMcpManager, MockInfra, MockMcpClient> {
        ForgeMcpService::new(Arc::new(MockMcpManager), Arc::new(MockInfra::default()))
    }

    #[tokio::test]
    async fn test_list_changed_relists_tools() {
        let infra = MockInfra::default();
        let service: ForgeMcpService<MockMcpManager, MockInfra, MockMcpClient> =
            ForgeMcpService::new(Arc::new(MockMcpManager), Arc::new(infra.clone()));
        let names = |servers: forge_app::domain::McpServers| -> Vec<String> {
            let mut names: Vec<String> = servers
                .get_servers()
                .values()
                .flatten()
                .map(|tool| tool.name.to_string())
                .collect();
            names.sort();
            names
        };
        let before = names(service.get_mcp_servers().await.unwrap());

        // The server adds a tool and sends notifications/tools/list_changed.
        infra
            .client
            .generation
            .store(1, std::sync::atomic::Ordering::SeqCst);
        infra
            .client
            .changed
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let after = names(service.get_mcp_servers().await.unwrap());

        assert_eq!(before, vec!["test_server__test_tool"]);
        assert_eq!(
            after,
            vec!["test_server__new_tool", "test_server__test_tool"]
        );
    }

    #[tokio::test]
    async fn test_tools_are_published_as_server_double_underscore_tool() {
        let service = fixture();

        let servers = service.get_mcp_servers().await.unwrap();
        let actual: Vec<String> = servers
            .get_servers()
            .values()
            .flatten()
            .map(|tool| tool.name.to_string())
            .collect();

        assert_eq!(actual, vec!["test_server__test_tool".to_string()]);
    }

    #[tokio::test]
    async fn test_incoming_claude_code_and_legacy_names_resolve() {
        let service = fixture();
        service.get_mcp_servers().await.unwrap();

        let claude = service
            .execute_mcp(ToolCallFull::new(ToolName::new(
                "mcp__test_server__test_tool",
            )))
            .await
            .unwrap();
        let legacy = service
            .execute_mcp(ToolCallFull::new(ToolName::new(
                "mcp_test_server_tool_test_tool",
            )))
            .await
            .unwrap();
        let missing = service
            .execute_mcp(ToolCallFull::new(ToolName::new("test_server__gone")))
            .await
            .unwrap_err()
            .to_string();

        assert_eq!(claude, ToolOutput::text("mock result"));
        assert_eq!(legacy, ToolOutput::text("mock result"));
        assert_eq!(missing, "MCP tool test_server__gone is no longer available");
    }

    #[tokio::test]
    async fn test_resource_read_is_redacted_and_unknown_server_refused() {
        let service = fixture();

        let read = service
            .mcp_control(
                "test-server",
                forge_app::McpControl::ReadResource { uri: "file:///notes.md".to_string() },
            )
            .await
            .unwrap();
        let unknown = service
            .mcp_control("nope", forge_app::McpControl::ListPrompts)
            .await
            .unwrap_err()
            .to_string();

        assert_eq!(
            read,
            serde_json::json!({"contents": [{"uri": "file:///notes.md", "text": "token=[REDACTED]"}]})
        );
        assert_eq!(unknown, "MCP server \"nope\" is not connected");
    }

    #[test]
    fn test_to_legacy_mcp_name_converts_claude_code_format() {
        let actual = ToolName::new("mcp__github__create_issue").to_legacy_mcp_name();
        let expected = Some(ToolName::new("mcp_github_tool_create_issue"));
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_to_legacy_mcp_name_converts_multipart_server_name() {
        let actual = ToolName::new("mcp__hugging_face__read_channel").to_legacy_mcp_name();
        let expected = Some(ToolName::new("mcp_hugging_face_tool_read_channel"));
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_to_legacy_mcp_name_returns_none_for_non_mcp_tools() {
        let actual = ToolName::new("read").to_legacy_mcp_name();
        assert_eq!(actual, None);
    }

    #[test]
    fn test_to_legacy_mcp_name_returns_none_for_legacy_format() {
        // Already in legacy format — should not double-convert
        let actual = ToolName::new("mcp_github_tool_create_issue").to_legacy_mcp_name();
        assert_eq!(actual, None);
    }

    // ── Concurrent initialisation test ──────────────────────────────────────

    /// Verify that two concurrent callers of `get_mcp_servers` do not race:
    /// after both futures settle, every registered tool must be callable
    /// without a "Tool not found" error.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_concurrent_init_does_not_race() {
        let service = Arc::new(fixture());

        let s1 = service.clone();
        let s2 = service.clone();
        let (r1, r2) = tokio::join!(s1.get_mcp_servers(), s2.get_mcp_servers());
        r1.unwrap();
        r2.unwrap();

        let servers = service.get_mcp_servers().await.unwrap();
        let tool_name = servers
            .get_servers()
            .values()
            .flat_map(|tools| tools.iter())
            .next()
            .expect("at least one tool must be registered")
            .name
            .clone();

        let call = ToolCallFull::new(tool_name);
        let actual = service.execute_mcp(call).await.unwrap();
        let expected = ToolOutput::text("mock result");
        assert_eq!(actual, expected);
    }
}
