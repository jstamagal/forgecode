Create a file, or replace one whole. Missing parent directories are created.
- Replacing an existing file needs `overwrite=true` and a prior read of the file in this conversation{{#if tool_names.read}} (`{{tool_names.read}}`){{/if}}; one page is enough, the whole file is not needed. Without `overwrite=true` an existing file is refused.
- If the file changed on disk since that read, the overwrite is refused and nothing is written; the error names both hashes and shows the diff from what you read to what is there now, and a retry after it goes through.
- The file keeps its existing line-ending style (LF, CRLF or CR); a new file gets LF.
- The previous state is snapshotted first{{#if tool_names.undo}}, so `{{tool_names.undo}}` brings it back (undoing a create deletes the file){{/if}}.
- After the write the file is parsed locally and syntax errors the write introduced are reported as a warning.
{{#if tool_names.patch}}
- Edit existing files with `{{tool_names.patch}}`; use this only for new files or a deliberate full rewrite.
{{/if}}
- Do not create documentation files unless asked.
