use std::path::PathBuf;
use std::sync::Arc;

use forge_domain::{ToolCallContext, ToolCatalog, ToolOutput};

use crate::fmt::content::FormatContent;
use crate::operation::ToolOperation;
use crate::services::{ReadRequest, Services, ShellService};
use crate::{
    AgentRegistry, ConversationService, EnvironmentInfra, FsPatchService, FsReadService,
    FsRemoveService, FsSearchService, FsUndoService, FsWriteService, ImageReadService,
    NetFetchService, PlanCreateService, ProviderService, SkillFetchService,
};

pub struct ToolExecutor<S> {
    services: Arc<S>,
}

impl<
    S: FsReadService
        + ImageReadService
        + FsWriteService
        + FsSearchService
        + NetFetchService
        + FsRemoveService
        + FsPatchService
        + FsUndoService
        + ShellService
        + ConversationService
        + EnvironmentInfra<Config = forge_config::ForgeConfig>
        + PlanCreateService
        + SkillFetchService
        + AgentRegistry
        + ProviderService
        + Services,
> ToolExecutor<S>
{
    pub fn new(services: Arc<S>) -> Self {
        Self { services }
    }

    /// Converts a path to absolute by joining it with the current working
    /// directory if it's relative
    fn normalize_path(&self, path: String) -> String {
        let env = self.services.get_environment();
        let path_buf = PathBuf::from(&path);

        if path_buf.is_absolute() {
            path
        } else {
            PathBuf::from(&env.cwd).join(path_buf).display().to_string()
        }
    }

    async fn call_internal(
        &self,
        input: ToolCatalog,
        context: &ToolCallContext,
    ) -> anyhow::Result<ToolOperation> {
        Ok(match input {
            ToolCatalog::Read(input) => {
                let normalized_path = self.normalize_path(input.file_path.clone());
                let range = input.range.clone().unwrap_or_default();
                let request = ReadRequest {
                    path: normalized_path,
                    start_line: range.start_line.map(|line| line.max(1) as u64),
                    end_line: range.end_line.map(|line| line.max(1) as u64),
                    start_byte: range.start_byte,
                    end_byte: range.end_byte,
                    show_line_numbers: input.show_line_numbers,
                    vision: context.vision(),
                };
                let output = self.services.read_tool(request, context).await?;

                (input, output).into()
            }
            ToolCatalog::Write(input) => {
                let normalized_path = self.normalize_path(input.file_path.clone());
                let output = self
                    .services
                    .write_tool(
                        normalized_path,
                        input.content.clone(),
                        input.overwrite,
                        context,
                    )
                    .await?;
                (input, output).into()
            }
            ToolCatalog::FsSearch(input) => {
                let mut params = input;
                // Resolve the search root against the working directory.
                let root = params
                    .path
                    .clone()
                    .filter(|path| !path.is_empty())
                    .unwrap_or_else(|| self.services.get_environment().cwd.display().to_string());
                params.path = Some(self.normalize_path(root));
                let text = self.services.search(params).await?;
                ToolOperation::Output { output: ToolOutput::text(text) }
            }
            ToolCatalog::AstSearch(input) => {
                let mut params = input;
                let root = params
                    .path
                    .clone()
                    .filter(|path| !path.is_empty())
                    .unwrap_or_else(|| self.services.get_environment().cwd.display().to_string());
                params.path = Some(self.normalize_path(root));
                let text = self.services.ast_search(params, context).await?;
                ToolOperation::Output { output: ToolOutput::text(text) }
            }
            ToolCatalog::Remove(input) => {
                let normalized_path = self.normalize_path(input.path.clone());
                let output = self
                    .services
                    .remove(normalized_path, input.permanent, context)
                    .await?;
                (input, output).into()
            }
            ToolCatalog::Patch(input) => {
                let normalized_path = self.normalize_path(input.file_path.clone());
                let edits = input.resolve_edits()?;
                let output = self
                    .services
                    .patch(normalized_path, edits, input.is_batch(), context)
                    .await?;
                (input, output).into()
            }
            ToolCatalog::Undo(input) => {
                let normalized_path = self.normalize_path(input.path.clone());
                let output = self.services.undo(normalized_path, context).await?;
                (input, output).into()
            }
            ToolCatalog::Shell(input) => {
                let cwd = input
                    .cwd
                    .clone()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| self.services.get_environment().cwd.display().to_string());
                let normalized_cwd = self.normalize_path(cwd);
                let text = self
                    .services
                    .shell_tool(input, PathBuf::from(normalized_cwd))
                    .await?;
                ToolOperation::Text { text }
            }
            ToolCatalog::Fetch(input) => {
                let save = input
                    .save
                    .clone()
                    .map(|path| PathBuf::from(self.normalize_path(path)));
                if let Some(target) = &save {
                    // A save overwrites; snapshot it so undo brings it back.
                    self.services.snapshot_before_write(target, context).await?;
                }
                let request = crate::services::FetchRequest {
                    url: input.url.clone(),
                    method: input.method(),
                    headers: input.headers.clone().unwrap_or_default(),
                    body: input.body.clone(),
                    json_body: input.json_body.clone(),
                    raw: input.raw.unwrap_or(false),
                    save,
                    vision: context.vision(),
                };
                let output = self.services.fetch_tool(request).await?;
                ToolOperation::Output { output }
            }
            ToolCatalog::Plan(input) => {
                let output = self
                    .services
                    .create_plan(
                        input.plan_name.clone(),
                        input.version.clone(),
                        input.content.clone(),
                    )
                    .await?;
                (input, output).into()
            }
            ToolCatalog::Skill(input) => {
                let skill = self.services.fetch_skill(input.name.clone()).await?;
                ToolOperation::Skill { output: skill }
            }
            ToolCatalog::TodoWrite(input) => {
                let before = context.get_todos()?;
                context.update_todos(input.todos.clone())?;
                let after = context.get_todos()?;
                ToolOperation::TodoWrite { before, after }
            }
            ToolCatalog::TodoRead(_input) => {
                let todos = context.get_todos()?;
                ToolOperation::TodoRead { output: todos }
            }
            ToolCatalog::Task(_) => {
                // Task tools are handled in ToolRegistry before reaching here
                unreachable!("Task tool should be handled in ToolRegistry")
            }
        })
    }

    pub async fn execute(
        &self,
        tool_input: ToolCatalog,
        context: &ToolCallContext,
    ) -> anyhow::Result<ToolOutput> {
        let tool_kind = tool_input.kind();
        let env = self.services.get_environment();

        let execution_result = self.call_internal(tool_input.clone(), context).await;

        if let Err(ref error) = execution_result {
            tracing::error!(error = ?error, "Tool execution failed");
        }

        let operation = execution_result?;

        // Send formatted output message
        if let Some(output) = operation.to_content(&env) {
            context.send(output).await?;
        }

        context.with_metrics(|metrics| operation.into_tool_output(tool_kind, &env, metrics))
    }
}
