Drive an interactive program through a persistent terminal (PTY): TUIs, REPLs, pagers, editors, installers and prompts. Sessions persist across calls for this conversation; a running process is normal, and stop sessions when you are done.{{#if tool_names.shell}} `{{tool_names.shell}}` is for ordinary non-interactive commands.{{/if}}
- `action`: `start` (needs `command`; `cols`/`rows` only here, default 120x40), `send` (`keys`), `look`, `stop`, `list`.
- `keys` is comma-separated: named keys are `enter`, `tab`, `esc`, the arrows (`up`, `down`, `left`, `right`), `home`, `end`, `pgup`, `pgdn`, `backspace`, `delete`, `ctrl-c`, `ctrl-d`, `ctrl-l`, `ctrl-x`, `ctrl-z` and `f1` to `f12`; anything else is typed verbatim, and `comma` types a comma.
- Each result has the rendered screen lines, cursor position and process state (`still_running`, `exit_status`). `lines_changed` and `screen_responded` compare against the previous observation: after `send`, change during the wait window after the keys (not proof the keys caused it); after `look`, passive change since last time.
{{#if tool_names.terminal_snapshot}}
- `{{tool_names.terminal_snapshot}}` renders the screen as a PNG when text cannot show spacing, colour, borders or clipping, and also resets that comparison baseline.
{{/if}}
- Under a jail (`-C DIR`) the command runs in the same jail as the shell tool.
