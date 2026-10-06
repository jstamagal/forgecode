Replace exact text in one file.
- Read the file{{#if tool_names.read}} with `{{tool_names.read}}`{{/if}} before editing it; editing a file this conversation has not read is refused. The `12:` prefix in read output is not file content; only the text after it is.
- `old_string` must match exactly once unless `replace_all` is set. When it is not in the file exactly, it is matched again with smart quotes, Unicode dashes, no-break spaces and trailing whitespace read as plain ASCII; the file keeps its own characters wherever your edit left `old_string` unchanged. With `replace_all`, this fallback runs only when there is no exact occurrence. A miss reports the closest line in the file.
- Pass `edits` (a list of `{old_string, new_string, replace_all}`) for several replacements in one call: they apply in order, each seeing the previous result, and all are validated before anything is written, so a failed call leaves the file untouched and names the offending edit. Never combine `edits` with the scalar `old_string`/`new_string`/`replace_all` form.
- Line endings in `old_string`/`new_string` are matched and written in the file's own style (LF or CRLF).
- After the write, the file is parsed locally and any syntax errors the edit introduced are reported as a warning.
{{#if tool_names.undo}}
- One call is one `{{tool_names.undo}}` step.
{{/if}}
{{#if tool_names.write}}
- Use `{{tool_names.write}}` for new files or a deliberate full rewrite.
{{/if}}
