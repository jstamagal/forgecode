use std::path::{Path, PathBuf};

use forge_domain::{ChatResponseContent, Environment, TitleFormat, ToolCatalog};

use crate::fmt::content::FormatContent;
use crate::utils::format_display_path;

impl FormatContent for ToolCatalog {
    fn to_content(&self, env: &Environment) -> Option<ChatResponseContent> {
        let display_path_for = |path: &str| format_display_path(Path::new(path), env.cwd.as_path());

        match self {
            ToolCatalog::Read(input) => {
                let display_path = display_path_for(&input.file_path);
                let is_explicit_range = input.range.is_some();
                let mut subtitle = display_path;
                if is_explicit_range && let Some(range) = &input.range {
                    match (range.start_line, range.end_line) {
                        (Some(start), Some(end)) => {
                            subtitle.push_str(&format!(":{start}-{end}"));
                        }
                        (Some(start), None) => {
                            subtitle.push_str(&format!(":{start}"));
                        }
                        (None, Some(end)) => {
                            subtitle.push_str(&format!(":1-{end}"));
                        }
                        (None, None) => {}
                    }
                };
                Some(TitleFormat::debug("Read").sub_title(subtitle).into())
            }
            ToolCatalog::Write(input) => {
                let path = PathBuf::from(&input.file_path);
                let display_path = display_path_for(&input.file_path);
                let title = match (path.exists(), input.overwrite) {
                    (true, true) => "Overwrite",
                    (true, false) => {
                        // Case: file exists but overwrite is false then we
                        // throw error from tool,
                        // so it's good idea to not print anything on CLI.
                        return None;
                    }
                    (false, _) => "Create",
                };
                Some(TitleFormat::debug(title).sub_title(display_path).into())
            }
            ToolCatalog::FsSearch(input) => {
                let formatted_dir = input.path.as_deref().unwrap_or(".");
                let formatted_dir = display_path_for(formatted_dir);

                let title = match (&input.glob, &input.file_type) {
                    (Some(glob), _) => {
                        format!(
                            "Search for '{}' in '{}' files at {}",
                            input.pattern, glob, formatted_dir
                        )
                    }
                    (None, Some(file_type)) => {
                        format!(
                            "Search for '{}' in {} files at {}",
                            input.pattern, file_type, formatted_dir
                        )
                    }
                    (None, None) => {
                        format!("Search for '{}' at {}", input.pattern, formatted_dir)
                    }
                };
                Some(TitleFormat::debug(title).into())
            }
            ToolCatalog::AstSearch(input) => {
                let at = display_path_for(input.path.as_deref().unwrap_or("."));
                let verb = match (&input.rewrite, input.apply) {
                    (Some(_), true) => "Rewrite",
                    (Some(_), false) => "Preview rewrite",
                    (None, _) => "AST search",
                };
                Some(
                    TitleFormat::debug(verb)
                        .sub_title(format!("{} at {at}", input.pattern))
                        .into(),
                )
            }
            ToolCatalog::Remove(input) => {
                let display_path = display_path_for(&input.path);
                Some(TitleFormat::debug("Remove").sub_title(display_path).into())
            }
            ToolCatalog::Patch(input) => {
                let display_path = display_path_for(&input.file_path);
                match &input.edits {
                    Some(edits) => Some(
                        TitleFormat::debug("Replace")
                            .sub_title(format!("{} ({} edits)", display_path, edits.len()))
                            .into(),
                    ),
                    None => {
                        let operation_name = if input.replace_all {
                            "Replace All"
                        } else {
                            "Replace"
                        };
                        Some(
                            TitleFormat::debug(operation_name)
                                .sub_title(display_path)
                                .into(),
                        )
                    }
                }
            }
            ToolCatalog::Undo(input) => {
                let display_path = display_path_for(&input.path);
                Some(TitleFormat::debug("Undo").sub_title(display_path).into())
            }
            ToolCatalog::Shell(input) => Some(
                TitleFormat::debug(format!("Execute [{}]", env.shell))
                    .sub_title(&input.command)
                    .into(),
            ),
            ToolCatalog::Fetch(input) => {
                Some(TitleFormat::debug("GET").sub_title(&input.url).into())
            }
            ToolCatalog::Plan(_) => None,
            ToolCatalog::Skill(input) => Some(
                TitleFormat::debug("Skill")
                    .sub_title(input.name.to_lowercase())
                    .into(),
            ),
            ToolCatalog::TodoWrite(input) => Some(
                TitleFormat::debug("Update Todos")
                    .sub_title(format!("{} item(s)", input.todos.len()))
                    .into(),
            ),
            ToolCatalog::TodoRead(_) => Some(TitleFormat::debug("Read Todos").into()),
            ToolCatalog::Task(input) => {
                Some(TitleFormat::debug("Task").sub_title(&input.agent_id).into())
            }
            ToolCatalog::ToolDiscovery(input) => Some(match &input.load {
                Some(id) => TitleFormat::debug("Load tool").sub_title(id).into(),
                None => TitleFormat::debug("Discover tools")
                    .sub_title(&input.query)
                    .into(),
            }),
            ToolCatalog::McpResourceList(input)
            | ToolCatalog::McpResourceTemplates(input)
            | ToolCatalog::McpPromptList(input) => Some(
                TitleFormat::debug(self.to_string())
                    .sub_title(&input.server)
                    .into(),
            ),
            ToolCatalog::McpResourceRead(input)
            | ToolCatalog::McpResourceSubscribe(input)
            | ToolCatalog::McpResourceUnsubscribe(input) => Some(
                TitleFormat::debug(self.to_string())
                    .sub_title(format!("{} {}", input.server, input.uri))
                    .into(),
            ),
            ToolCatalog::McpPromptGet(input) => Some(
                TitleFormat::debug(self.to_string())
                    .sub_title(format!("{} {}", input.server, input.name))
                    .into(),
            ),
            ToolCatalog::TerminalSession(input) => Some(
                TitleFormat::debug(format!("Terminal {}", input.action))
                    .sub_title(
                        input
                            .command
                            .clone()
                            .or_else(|| input.keys.clone())
                            .unwrap_or_else(|| {
                                input.session.clone().unwrap_or_else(|| "main".to_string())
                            }),
                    )
                    .into(),
            ),
            ToolCatalog::TerminalSnapshot(input) => Some(
                TitleFormat::debug("Terminal snapshot")
                    .sub_title(input.session.clone().unwrap_or_else(|| "main".to_string()))
                    .into(),
            ),
            ToolCatalog::Lsp(input) => Some(
                TitleFormat::debug(format!("LSP {}", input.operation))
                    .sub_title(display_path_for(&input.file_path))
                    .into(),
            ),
            ToolCatalog::Kernel(input) => Some(
                TitleFormat::debug(format!(
                    "Kernel {}",
                    input.action.as_deref().unwrap_or("run")
                ))
                .sub_title(
                    input
                        .code
                        .as_deref()
                        .and_then(|c| c.lines().next())
                        .unwrap_or_default(),
                )
                .into(),
            ),
            ToolCatalog::Toolbox(input) => Some(
                TitleFormat::debug(format!(
                    "Toolbox {}",
                    input.action.as_deref().unwrap_or("list")
                ))
                .sub_title(input.name.clone().unwrap_or_default())
                .into(),
            ),
            ToolCatalog::Browse(input) => {
                Some(TitleFormat::debug("Browse").sub_title(&input.url).into())
            }
            ToolCatalog::BrowserProbe(input) => Some(
                TitleFormat::debug("Browser probe")
                    .sub_title(&input.target)
                    .into(),
            ),
            ToolCatalog::SerperSearch(input) => Some(
                TitleFormat::debug("Serper search")
                    .sub_title(&input.query)
                    .into(),
            ),
            ToolCatalog::TavilySearch(input) => Some(
                TitleFormat::debug("Tavily search")
                    .sub_title(&input.query)
                    .into(),
            ),
            ToolCatalog::ExaSearch(input) => Some(
                TitleFormat::debug("Exa search")
                    .sub_title(&input.query)
                    .into(),
            ),
            ToolCatalog::DocsSearch(input) => Some(
                TitleFormat::debug("Docs search")
                    .sub_title(&input.library)
                    .into(),
            ),
            ToolCatalog::NotebookEdit(input) => Some(
                TitleFormat::debug(format!(
                    "Notebook {}",
                    input.edit_mode.as_deref().unwrap_or("replace")
                ))
                .sub_title(display_path_for(&input.file_path))
                .into(),
            ),
            ToolCatalog::WikiConvert(input) => Some(
                TitleFormat::debug("Wiki convert")
                    .sub_title(display_path_for(&input.file_path))
                    .into(),
            ),
            ToolCatalog::WikiWrite(input) => Some(
                TitleFormat::debug("Wiki write")
                    .sub_title(&input.title)
                    .into(),
            ),
            ToolCatalog::WikiFinishIngest(input) => Some(
                TitleFormat::debug("Wiki finish ingest")
                    .sub_title(&input.source)
                    .into(),
            ),
        }
    }
}
