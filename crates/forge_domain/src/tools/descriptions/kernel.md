Execute Python in a persistent IPython kernel (Jupyter). State survives between calls: functions, imports, open connections and loaded data defined in one call are there in the next.
- `action`: `run` (default) submits `code`; `poll` returns what a running cell produced since the last read; `wait` blocks for it up to `timeout`; `interrupt` stops the cell (the namespace survives); `restart` replaces the kernel (destroys every definition); `kill` shuts it down.
- `timeout` (default 120 seconds) is the longest one call blocks. A cell that finishes inside it comes back whole; one still running comes back as a handle, `HANDLE <n> RUNNING`, and a new `code` is refused while it runs.
- stdout, stderr, results and tracebacks come back; images are written to files and reported as `IMAGE <path>`. A finished cell ends with a `NAMESPACE` line listing the functions and classes defined so far.
- Cells can call tools: the namespace holds `tools`, and `tools.<name>(...)` runs the tool `<name>` with keyword arguments and returns its result text. Each call goes through the same checks as a direct call (allowed tools, argument validation, bans, read-before-edit); a refusal raises `tools.ToolError`. `tools.names()` lists the callable tools; `kernel` and `toolbox` are not callable from a cell.
{{#if tool_names.toolbox}}
- `{{tool_names.toolbox}}` saves functions defined here to disk and loads them into later sessions.
{{/if}}
{{#if tool_names.shell}}
- Use `{{tool_names.shell}}` for builds, tests, git and anything that is a command rather than Python.
{{/if}}
