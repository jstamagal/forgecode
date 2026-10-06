Change one cell of a Jupyter notebook (.ipynb): `replace` its source (the default), `insert` a new cell after it, or `delete` it. Read the notebook first{{#if tool_names.read}}; `{{tool_names.read}}` shows each cell's id as `[cell N id=ID type]`{{/if}}. A notebook changed on disk since the read is refused; read it again.
- `cell_id` is the cell's id, or `index:N` (0-based) for notebooks without ids. `insert` with no `cell_id` inserts at the top and needs `cell_type` (`code` or `markdown`).
- `new_source` is the cell's whole new source. A replaced code cell loses its outputs and execution count.
- The notebook keeps its JSON layout (one-space indent, trailing newline).
{{#if tool_names.undo}}
- The notebook is snapshotted first, so `{{tool_names.undo}}` restores it.
{{/if}}
