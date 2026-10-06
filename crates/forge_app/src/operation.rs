use std::path::Path;

use console::strip_ansi_codes;
use forge_display::DiffFormat;
use forge_domain::{
    Environment, FSPatch, FSRead, FSRemove, FSUndo, FSWrite, FileOperation, LineNumbers, Metrics,
    PlanCreate, ToolKind,
};
use forge_template::Element;

use crate::utils::{compute_hash, format_display_path};
use crate::{
    Content, FsRemoveOutput, FsUndoOutput, FsWriteOutput, PatchOutput, PlanCreateOutput,
    ReadOutput, RemoveMode, UndoKind,
};

/// `<warning>` elements for undo-store notices (e.g. a snapshot kept only in
/// memory, or one that could not be captured).
fn notice_elements(notices: &[String]) -> Vec<Element> {
    notices
        .iter()
        .map(|notice| Element::new("warning").text(notice))
        .collect()
}

#[derive(Debug, derive_more::From)]
pub enum ToolOperation {
    FsRead {
        input: FSRead,
        output: ReadOutput,
    },
    FsWrite {
        input: FSWrite,
        output: FsWriteOutput,
    },
    FsRemove {
        input: FSRemove,
        output: FsRemoveOutput,
    },
    FsPatch {
        input: FSPatch,
        output: PatchOutput,
    },
    FsUndo {
        input: FSUndo,
        output: FsUndoOutput,
    },
    /// Finished model-facing text a tool rendered itself.
    Text {
        text: String,
    },
    /// A finished tool output (text and/or images) a tool built itself.
    Output {
        output: forge_domain::ToolOutput,
    },
    PlanCreate {
        input: PlanCreate,
        output: PlanCreateOutput,
    },
    Skill {
        output: forge_domain::Skill,
    },
    TodoWrite {
        before: Vec<forge_domain::Todo>,
        after: Vec<forge_domain::Todo>,
    },
    TodoRead {
        output: Vec<forge_domain::Todo>,
    },
}

/// Creates a validation warning element for syntax errors the edit introduced
///
/// # Arguments
/// * `path` - The file path
/// * `errors` - Vector of syntax errors
///
/// Returns an Element containing the formatted warning with all error details
fn create_validation_warning(path: &str, errors: &[forge_domain::SyntaxError]) -> Element {
    Element::new("warning")
        .append(Element::new("message").text("Syntax validation failed"))
        .append(Element::new("file").attr("path", path))
        .append(Element::new("details").text(format!(
            "The file was written successfully but the edit introduced {} syntax error(s)",
            errors.len()
        )))
        .append(errors.iter().map(|error| {
            Element::new("error")
                .attr("line", error.line.to_string())
                .attr("column", error.column.to_string())
                .cdata(&error.message)
        }))
        .append(Element::new("suggestion").text("Review and fix the syntax issues"))
}

impl ToolOperation {
    /// Converts this tool operation into a [`forge_domain::ToolOutput`].
    ///
    /// # Arguments
    /// * `tool_kind` - The kind of tool that produced this operation.
    /// * `env` - Slim runtime environment (used for `cwd` and `shell`).
    /// * `metrics` - Mutable reference to the conversation metrics that will be
    ///   updated.
    pub fn into_tool_output(
        self,
        tool_kind: ToolKind,
        env: &Environment,
        metrics: &mut Metrics,
    ) -> forge_domain::ToolOutput {
        let tool_name = tool_kind.name();
        match self {
            ToolOperation::FsRead { input, output } => {
                // Track read operations (the external-change detector compares
                // this hash with the file on disk between turns).
                tracing::info!(path = %input.file_path, tool = %tool_name, "File read");
                if !output.info.content_hash.is_empty() {
                    *metrics = metrics.clone().insert(
                        input.file_path.clone(),
                        FileOperation::new(tool_kind)
                            .content_hash(Some(output.info.content_hash.clone())),
                    );
                }

                match output.content {
                    Content::Image(image) => forge_domain::ToolOutput::image(image),
                    Content::Rendered(rendered) => forge_domain::ToolOutput::text(rendered.text),
                    Content::File(content) => {
                        let content = if input.show_line_numbers {
                            content
                                .to_numbered_from(output.info.start_line as usize)
                                .to_string()
                        } else {
                            content
                        };
                        forge_domain::ToolOutput::text(content)
                    }
                }
            }
            ToolOperation::FsWrite { input, output } => {
                let diff_result = DiffFormat::format(
                    output.before.as_ref().unwrap_or(&"".to_string()),
                    &input.content,
                );
                let diff = console::strip_ansi_codes(diff_result.diff()).to_string();

                *metrics = metrics.clone().insert(
                    input.file_path.clone(),
                    FileOperation::new(tool_kind)
                        .lines_added(diff_result.lines_added())
                        .lines_removed(diff_result.lines_removed())
                        .content_hash(Some(output.content_hash.clone())),
                );

                let mut elm = if output.before.as_ref().is_some() {
                    Element::new("file_overwritten").append(Element::new("file_diff").cdata(diff))
                } else {
                    Element::new("file_created")
                };

                elm = elm
                    .attr("path", &input.file_path)
                    .attr("total_lines", input.content.lines().count());

                if !output.errors.is_empty() {
                    elm = elm.append(create_validation_warning(&input.file_path, &output.errors));
                }
                elm = elm.append(notice_elements(&output.notices));

                forge_domain::ToolOutput::text(elm)
            }
            ToolOperation::FsRemove { input, output } => {
                // None since file was removed
                let content_hash = None;

                *metrics = metrics.clone().insert(
                    input.path.clone(),
                    FileOperation::new(tool_kind)
                        .lines_removed(output.content.lines().count() as u64)
                        .content_hash(content_hash),
                );

                let display_path = format_display_path(Path::new(&input.path), env.cwd.as_path());
                let status = match output.mode {
                    RemoveMode::Trashed => "trashed",
                    RemoveMode::Deleted => "deleted",
                };
                let elem = Element::new("file_removed")
                    .attr("path", display_path)
                    .attr("status", status)
                    .append(notice_elements(&output.notices));
                forge_domain::ToolOutput::text(elem)
            }
            ToolOperation::FsPatch { input, output } => {
                let diff_result = DiffFormat::format(&output.before, &output.after);
                let diff = console::strip_ansi_codes(diff_result.diff()).to_string();

                let mut summary = if output.batch {
                    format!(
                        "{} edit{}",
                        output.edits,
                        if output.edits == 1 { "" } else { "s" }
                    )
                } else {
                    format!(
                        "{} replacement{}",
                        output.replacements,
                        if output.replacements == 1 { "" } else { "s" }
                    )
                };
                if output.fuzzy_edits > 0 {
                    let who = if output.batch {
                        format!("{} of them", output.fuzzy_edits)
                    } else {
                        "old_string".to_string()
                    };
                    summary.push_str(&format!(
                        ", {who} matched after normalising quotes, dashes and whitespace"
                    ));
                }

                let mut elm = Element::new("file_diff")
                    .attr("path", &input.file_path)
                    .attr("total_lines", output.after.lines().count())
                    .attr("summary", summary)
                    .cdata(diff);

                if !output.errors.is_empty() {
                    elm = elm.append(create_validation_warning(&input.file_path, &output.errors));
                }
                elm = elm.append(notice_elements(&output.notices));

                *metrics = metrics.clone().insert(
                    input.file_path.clone(),
                    FileOperation::new(tool_kind)
                        .lines_added(diff_result.lines_added())
                        .lines_removed(diff_result.lines_removed())
                        .content_hash(Some(output.content_hash.clone())),
                );

                forge_domain::ToolOutput::text(elm)
            }
            ToolOperation::FsUndo { input, output } => {
                // Diff between snapshot state (after_undo) and modified state
                // (before_undo)
                let diff = DiffFormat::format(
                    output.after_undo.as_deref().unwrap_or(""),
                    output.before_undo.as_deref().unwrap_or(""),
                );
                let content_hash = output.after_undo.as_ref().map(|s| compute_hash(s));

                *metrics = metrics.clone().insert(
                    input.path.clone(),
                    FileOperation::new(tool_kind)
                        .lines_added(diff.lines_added())
                        .lines_removed(diff.lines_removed())
                        .content_hash(content_hash),
                );

                match (&output.kind, &output.before_undo, &output.after_undo) {
                    (UndoKind::Directory, _, _) => {
                        let elm = Element::new("file_undo")
                            .attr("path", input.path)
                            .attr("status", "restored_directory");
                        forge_domain::ToolOutput::text(elm)
                    }
                    (UndoKind::Symlink { target }, _, _) => {
                        let elm = Element::new("file_undo")
                            .attr("path", input.path)
                            .attr("status", "restored_symlink")
                            .attr("target", target);
                        forge_domain::ToolOutput::text(elm)
                    }
                    (UndoKind::Deletion, _, _) => {
                        let elm = Element::new("file_undo")
                            .attr("path", input.path)
                            .attr("status", "removed");
                        forge_domain::ToolOutput::text(elm)
                    }
                    (UndoKind::File, None, None) => {
                        let elm = Element::new("file_undo")
                            .attr("path", input.path)
                            .attr("status", "no_changes");
                        forge_domain::ToolOutput::text(elm)
                    }
                    (UndoKind::File, None, Some(after)) => {
                        let elm = Element::new("file_undo")
                            .attr("path", input.path)
                            .attr("status", "created")
                            .attr("total_lines", after.lines().count())
                            .cdata(after);
                        forge_domain::ToolOutput::text(elm)
                    }
                    (UndoKind::File, Some(before), None) => {
                        let elm = Element::new("file_undo")
                            .attr("path", input.path)
                            .attr("status", "removed")
                            .attr("total_lines", before.lines().count())
                            .cdata(before);
                        forge_domain::ToolOutput::text(elm)
                    }
                    (UndoKind::File, Some(before), Some(after)) => {
                        // This diff is between modified state (before_undo) and
                        // snapshot state (after_undo)
                        let diff = DiffFormat::format(before, after);

                        let elm = Element::new("file_undo")
                            .attr("path", input.path)
                            .attr("status", "restored")
                            .cdata(strip_ansi_codes(diff.diff()));

                        forge_domain::ToolOutput::text(elm)
                    }
                }
            }
            ToolOperation::Text { text } => forge_domain::ToolOutput::text(text),
            ToolOperation::Output { output } => output,
            ToolOperation::PlanCreate { input, output } => {
                let elm = Element::new("plan_created")
                    .attr("path", output.path.display().to_string())
                    .attr("plan_name", input.plan_name)
                    .attr("version", input.version);

                forge_domain::ToolOutput::text(elm)
            }
            ToolOperation::Skill { output } => {
                let mut elm = Element::new("skill_details");

                elm = elm.append({
                    let mut elm = Element::new("command");
                    if let Some(path) = output.path {
                        elm = elm.attr("location", path.display().to_string());
                    }

                    elm.cdata(output.command)
                });

                // Insert Resources
                if !output.resources.is_empty() {
                    elm = elm.append(output.resources.iter().map(|resource| {
                        Element::new("resource").text(resource.display().to_string())
                    }));
                }

                forge_domain::ToolOutput::text(elm)
            }
            ToolOperation::TodoWrite { before, after } => {
                // Build a map of before todos by ID for diff computation
                let before_map: std::collections::HashMap<&str, &forge_domain::Todo> =
                    before.iter().map(|t| (t.id.as_str(), t)).collect();

                let mut added = Vec::new();
                let mut updated = Vec::new();

                for todo in &after {
                    match before_map.get(todo.id.as_str()) {
                        None => added.push(todo),
                        Some(prev)
                            if prev.status != todo.status || prev.content != todo.content =>
                        {
                            updated.push((prev, todo))
                        }
                        _ => {}
                    }
                }

                let after_ids: std::collections::HashSet<&str> =
                    after.iter().map(|t| t.id.as_str()).collect();
                let removed: Vec<_> = before
                    .iter()
                    .filter(|t| !after_ids.contains(t.id.as_str()))
                    .collect();

                let total_changes = added.len() + updated.len() + removed.len();
                let mut elm = Element::new("todos_updated").attr("changes", total_changes);

                for todo in added {
                    let todo_elm = Element::new("todo")
                        .attr("status", todo.status.to_string())
                        .attr("change", "added")
                        .text(&todo.content);
                    elm = elm.append(todo_elm);
                }

                for (prev, todo) in updated {
                    let mut todo_elm = Element::new("todo")
                        .attr("status", todo.status.to_string())
                        .attr("change", "updated");
                    if prev.status != todo.status {
                        todo_elm = todo_elm
                            .attr("prev_status", prev.status.to_string())
                            .attr("new_status", todo.status.to_string());
                    }
                    todo_elm = todo_elm.text(&todo.content);
                    elm = elm.append(todo_elm);
                }

                for todo in removed {
                    let todo_elm = Element::new("todo")
                        .attr("status", todo.status.to_string())
                        .attr("change", "removed")
                        .text(&todo.content);
                    elm = elm.append(todo_elm);
                }

                forge_domain::ToolOutput::text(elm)
            }
            ToolOperation::TodoRead { output } => {
                let mut elm = Element::new("todos").attr("count", output.len());

                for todo in output {
                    let todo_elm = Element::new("todo")
                        .attr("status", todo.status.to_string())
                        .text(&todo.content);
                    elm = elm.append(todo_elm);
                }

                forge_domain::ToolOutput::text(elm)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fmt::Write;
    use std::path::PathBuf;

    use forge_domain::{FSRead, FSReadRange, FileInfo, ToolValue};

    use super::*;
    use crate::Content;

    fn fixture_environment() -> Environment {
        use fake::{Fake, Faker};
        let fixture: Environment = Faker.fake();
        fixture.cwd(PathBuf::from("/projects/test")) // Set deterministic cwd to avoid flaky path formatting
    }

    fn to_value(output: forge_domain::ToolOutput) -> String {
        let values = output.values;
        let mut result = String::new();
        values.into_iter().for_each(|value| match value {
            ToolValue::Text(txt) => {
                writeln!(result, "{}", txt).unwrap();
            }
            ToolValue::Image(image) => {
                writeln!(result, "Image with mime type: {}", image.mime_type()).unwrap();
            }
            ToolValue::Empty => {
                writeln!(result, "Empty value").unwrap();
            }
            ToolValue::AI { value, .. } => {
                writeln!(result, "{}", value).unwrap();
            }
        });

        result
    }

    /// Creates test syntax errors for testing purposes
    fn test_syntax_errors(errors: Vec<(u32, u32, &str)>) -> Vec<forge_domain::SyntaxError> {
        use forge_domain::SyntaxError;

        errors
            .into_iter()
            .map(|(line, column, message)| SyntaxError {
                line,
                column,
                message: message.to_string(),
            })
            .collect()
    }

    #[test]
    fn test_fs_read_basic() {
        let content = "Hello, world!\nThis is a test file.";
        let hash = crate::compute_hash(content);
        let fixture = ToolOperation::FsRead {
            input: FSRead {
                file_path: "/home/user/test.txt".to_string(),
                range: None,
                show_line_numbers: true,
            },
            output: ReadOutput {
                content: Content::file(content),
                info: FileInfo::new(1, 2, 2, hash),
            },
        };

        let env = fixture_environment();

        let actual = fixture.into_tool_output(ToolKind::Read, &env, &mut Metrics::default());

        insta::assert_snapshot!(to_value(actual));
    }

    #[test]
    fn test_fs_read_basic_special_chars() {
        let content = "struct Foo<T>{ name: T }";
        let hash = crate::compute_hash(content);
        let fixture = ToolOperation::FsRead {
            input: FSRead {
                file_path: "/home/user/test.txt".to_string(),
                range: None,
                show_line_numbers: true,
            },
            output: ReadOutput {
                content: Content::file(content),
                info: FileInfo::new(1, 1, 1, hash),
            },
        };

        let env = fixture_environment();
        let actual = fixture.into_tool_output(ToolKind::Read, &env, &mut Metrics::default());

        insta::assert_snapshot!(to_value(actual));
    }

    #[test]
    fn test_fs_read_with_explicit_range() {
        let content = "Line 1\nLine 2\nLine 3";
        let hash = crate::compute_hash(content);
        let fixture = ToolOperation::FsRead {
            input: FSRead {
                file_path: "/home/user/test.txt".to_string(),
                range: Some(FSReadRange {
                    start_line: Some(2),
                    end_line: Some(3),
                    ..Default::default()
                }),
                show_line_numbers: true,
            },
            output: ReadOutput {
                content: Content::file(content),
                info: FileInfo::new(2, 3, 5, hash),
            },
        };

        let env = fixture_environment();

        let actual = fixture.into_tool_output(ToolKind::Read, &env, &mut Metrics::default());

        insta::assert_snapshot!(to_value(actual));
    }

    #[test]
    fn test_fs_create_basic() {
        let content = "Hello, world!";
        let fixture = ToolOperation::FsWrite {
            input: forge_domain::FSWrite {
                file_path: "/home/user/new_file.txt".to_string(),
                content: content.to_string(),
                overwrite: false,
            },
            output: FsWriteOutput {
                path: "/home/user/new_file.txt".to_string(),
                before: None,
                errors: vec![],
                content_hash: compute_hash(content),
                bytes: 0,
                notices: vec![],
            },
        };

        let env = fixture_environment();

        let actual = fixture.into_tool_output(ToolKind::Write, &env, &mut Metrics::default());

        insta::assert_snapshot!(to_value(actual));
    }

    #[test]
    fn test_fs_create_overwrite() {
        let content = "New content for the file";
        let fixture = ToolOperation::FsWrite {
            input: forge_domain::FSWrite {
                file_path: "/home/user/existing_file.txt".to_string(),
                content: content.to_string(),
                overwrite: true,
            },
            output: FsWriteOutput {
                path: "/home/user/existing_file.txt".to_string(),
                before: Some("Old content".to_string()),
                errors: vec![],
                content_hash: compute_hash(content),
                bytes: 0,
                notices: vec![],
            },
        };

        let env = fixture_environment();
        let actual = fixture.into_tool_output(ToolKind::Write, &env, &mut Metrics::default());

        insta::assert_snapshot!(to_value(actual));
    }

    #[test]
    fn test_fs_create_with_warning() {
        let content = "Content with warning";
        let fixture = ToolOperation::FsWrite {
            input: forge_domain::FSWrite {
                file_path: "/home/user/file_with_warning.txt".to_string(),
                content: content.to_string(),
                overwrite: false,
            },
            output: FsWriteOutput {
                path: "/home/user/file_with_warning.txt".to_string(),
                before: None,
                errors: test_syntax_errors(vec![(10, 5, "Syntax error on line 10")]),
                content_hash: compute_hash(content),
                bytes: 0,
                notices: vec![],
            },
        };

        let env = fixture_environment();

        let actual = fixture.into_tool_output(ToolKind::Write, &env, &mut Metrics::default());

        insta::assert_snapshot!(to_value(actual));
    }

    #[test]
    fn test_fs_create_with_warning_xml_tags() {
        let content = "Content with warning";
        let fixture = ToolOperation::FsWrite {
            input: forge_domain::FSWrite {
                file_path: "/home/user/file_with_warning.txt".to_string(),
                content: content.to_string(),
                overwrite: false,
            },
            output: FsWriteOutput {
                path: "/home/user/file_with_warning.txt".to_string(),
                before: None,
                errors: test_syntax_errors(vec![
                    (10, 5, "Syntax error on line 10"),
                    (20, 15, "Missing semicolon"),
                ]),
                content_hash: compute_hash(content),
                bytes: 0,
                notices: vec![],
            },
        };

        let env = fixture_environment();

        let actual = fixture.into_tool_output(ToolKind::Write, &env, &mut Metrics::default());

        insta::assert_snapshot!(to_value(actual));
    }

    #[test]
    fn test_fs_remove_success() {
        let fixture = ToolOperation::FsRemove {
            input: forge_domain::FSRemove::new("/home/user/file_to_delete.txt".to_string()),
            output: FsRemoveOutput {
                content: "content".to_string(),
                mode: crate::RemoveMode::Deleted,
                notices: vec![],
            },
        };

        let env = fixture_environment();

        let actual = fixture.into_tool_output(ToolKind::Remove, &env, &mut Metrics::default());

        insta::assert_snapshot!(to_value(actual));
    }

    #[test]
    fn test_fs_patch_basic() {
        let after_content = "Hello universe\nThis is a test";
        let fixture = ToolOperation::FsPatch {
            input: forge_domain::FSPatch::new("/home/user/test.txt", "world", "universe"),
            output: PatchOutput {
                errors: vec![],
                before: "Hello world\nThis is a test".to_string(),
                after: after_content.to_string(),
                content_hash: compute_hash(after_content),
                edits: 1,
                replacements: 1,
                fuzzy_edits: 0,
                batch: false,
                notices: vec![],
            },
        };

        let env = fixture_environment();

        let actual = fixture.into_tool_output(ToolKind::Patch, &env, &mut Metrics::default());

        insta::assert_snapshot!(to_value(actual));
    }

    #[test]
    fn test_fs_patch_with_warning() {
        let after_content = "line1\nnew line\nline2";
        let fixture = ToolOperation::FsPatch {
            input: forge_domain::FSPatch::new("/home/user/large_file.txt", "line1", "\nnew line"),
            output: PatchOutput {
                errors: test_syntax_errors(vec![(5, 10, "Invalid syntax")]),
                before: "line1\nline2".to_string(),
                after: after_content.to_string(),
                content_hash: compute_hash(after_content),
                edits: 1,
                replacements: 1,
                fuzzy_edits: 0,
                batch: false,
                notices: vec![],
            },
        };

        let env = fixture_environment();

        let actual = fixture.into_tool_output(ToolKind::Patch, &env, &mut Metrics::default());

        insta::assert_snapshot!(to_value(actual));
    }

    #[test]
    fn test_fs_patch_with_warning_special_chars() {
        let after_content = "line1\nnew line\nline2";
        let fixture = ToolOperation::FsPatch {
            input: forge_domain::FSPatch::new("/home/user/test.zsh", "line1", "\nnew line"),
            output: PatchOutput {
                errors: test_syntax_errors(vec![
                    (
                        22,
                        1,
                        r#"Syntax error at 'function dim() { echo "${_DIM}${1}${RESET}"'"#,
                    ),
                    (25, 5, "Unexpected token"),
                    (30, 10, "Missing closing brace"),
                ]),
                before: "line1\nline2".to_string(),
                after: after_content.to_string(),
                content_hash: compute_hash(after_content),
                edits: 1,
                replacements: 1,
                fuzzy_edits: 0,
                batch: false,
                notices: vec![],
            },
        };

        let env = fixture_environment();

        let actual = fixture.into_tool_output(ToolKind::Patch, &env, &mut Metrics::default());

        insta::assert_snapshot!(to_value(actual));
    }

    #[test]
    fn test_fs_undo_no_changes() {
        let fixture = ToolOperation::FsUndo {
            input: forge_domain::FSUndo { path: "/home/user/unchanged_file.txt".to_string() },
            output: FsUndoOutput {
                before_undo: None,
                after_undo: None,
                kind: crate::UndoKind::File,
            },
        };

        let env = fixture_environment();

        let actual = fixture.into_tool_output(ToolKind::Undo, &env, &mut Metrics::default());

        insta::assert_snapshot!(to_value(actual));
    }

    #[test]
    fn test_fs_undo_file_created() {
        let fixture = ToolOperation::FsUndo {
            input: forge_domain::FSUndo { path: "/home/user/new_file.txt".to_string() },
            output: FsUndoOutput {
                before_undo: None,
                after_undo: Some("New file content\nLine 2\nLine 3".to_string()),
                kind: crate::UndoKind::File,
            },
        };

        let env = fixture_environment();

        let actual = fixture.into_tool_output(ToolKind::Undo, &env, &mut Metrics::default());

        insta::assert_snapshot!(to_value(actual));
    }

    #[test]
    fn test_fs_undo_file_removed() {
        let fixture = ToolOperation::FsUndo {
            input: forge_domain::FSUndo { path: "/home/user/deleted_file.txt".to_string() },
            output: FsUndoOutput {
                before_undo: Some(
                    "Original file content\nThat was deleted\nDuring undo".to_string(),
                ),
                after_undo: None,
                kind: crate::UndoKind::File,
            },
        };

        let env = fixture_environment();

        let actual = fixture.into_tool_output(ToolKind::Undo, &env, &mut Metrics::default());

        insta::assert_snapshot!(to_value(actual));
    }

    #[test]
    fn test_fs_undo_file_restored() {
        let fixture = ToolOperation::FsUndo {
            input: forge_domain::FSUndo { path: "/home/user/restored_file.txt".to_string() },
            output: FsUndoOutput {
                before_undo: Some("Original content\nBefore changes".to_string()),
                after_undo: Some("Modified content\nAfter restoration".to_string()),
                kind: crate::UndoKind::File,
            },
        };

        let env = fixture_environment();

        let actual = fixture.into_tool_output(ToolKind::Undo, &env, &mut Metrics::default());

        insta::assert_snapshot!(to_value(actual));
    }

    #[test]
    fn test_fs_undo_success() {
        let fixture = ToolOperation::FsUndo {
            input: forge_domain::FSUndo { path: "/home/user/test.txt".to_string() },
            output: FsUndoOutput {
                before_undo: Some("ABC".to_string()),
                after_undo: Some("PQR".to_string()),
                kind: crate::UndoKind::File,
            },
        };

        let env = fixture_environment();

        let actual = fixture.into_tool_output(ToolKind::Undo, &env, &mut Metrics::default());

        insta::assert_snapshot!(to_value(actual));
    }

    #[test]
    fn test_skill_operation() {
        let fixture = ToolOperation::Skill {
            output: forge_domain::Skill::new(
                "test-skill",
                "This is a test skill command with instructions",
                "A test skill for demonstration",
            )
            .path("/home/user/.forge/skills/test-skill")
            .resources(vec![
                PathBuf::from("/home/user/.forge/skills/test-skill/resource1.txt"),
                PathBuf::from("/home/user/.forge/skills/test-skill/resource2.md"),
            ]),
        };

        let env = fixture_environment();

        let actual = fixture.into_tool_output(ToolKind::Skill, &env, &mut Metrics::default());

        insta::assert_snapshot!(to_value(actual));
    }

    #[test]
    fn test_fs_read_image_with_vision_model() {
        use forge_domain::Image;

        let fixture = ToolOperation::FsRead {
            input: FSRead {
                file_path: "/home/user/test.png".to_string(),
                range: None,
                show_line_numbers: true,
            },
            output: ReadOutput {
                content: Content::image(Image::new_base64(
                    "base64_image_data".to_string(),
                    "image/png",
                )),
                info: FileInfo::new(1, 1, 1, "hash123".to_string()),
            },
        };

        let env = fixture_environment();
        let actual = fixture.into_tool_output(ToolKind::Read, &env, &mut Metrics::default());

        // Should return image content
        assert!(!actual.values.is_empty(), "Expected non-empty output");
        match &actual.values[0] {
            forge_domain::ToolValue::Image(_) => (), // Expected
            _ => panic!("Expected image output for vision model"),
        }
    }
}
