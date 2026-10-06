Search file contents by regex. `output_mode` is `files_with_matches` (paths, the default), `content` (matching lines as `path:line:text`, context lines as `path-line-text`, `--` between groups) or `count` (`path:N`).
- The search uses ripgrep's libraries in-process. Paths ignored by `.gitignore`/`.ignore` are skipped, as are hidden paths unless `path` names one or `glob` names a dot-leading component such as `.github/**`; binary files are skipped.
- `glob` filters paths (a leading `!` excludes); `file_type` takes ripgrep type names (`rust`, `py`, `js`, …) or a bare extension, and cannot be combined with a positive `glob`.
- Regex syntax is ripgrep's: escape literal braces (`interface\{\}`); `multiline: true` lets a pattern span lines.
- Results are absolute paths in path order. Bytes that are not valid UTF-8 are shown as `\xNN`.
- The walk stays on the filesystem `path` is on and does not enter mount points below it; name a mounted share as `path` to search it. A search is bounded at 120 seconds.
- `head_limit` and `offset` page through a large result; the result names the next `offset` when there are more. A result too large to show inline is written to a file whose path the result names.
- Use `{{tool_names.read}}` to see a file once a search has found it, and `{{tool_names.task}}` for open-ended investigations that need several rounds of search and reading.
