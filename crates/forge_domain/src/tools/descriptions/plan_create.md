Write a markdown plan to `plans/{date}-{plan_name}-{version}.md` under the working directory (or the configured plans directory).
- Use it for durable implementation plans, task breakdowns, investigation notes or strategy documents that should outlive this turn, or when the user asks for a plan to be written down. Do not create documentation proactively.
- `plan_name` and `version` are sanitised into the filename: anything other than letters, digits, `-`, `_` and `.` becomes `-`.
- `content` is written verbatim.
- An existing plan with the same date, name and version is left unchanged unless `overwrite=true`; an overwrite reports that it replaced the plan{{#if tool_names.undo}}, and the previous contents are snapshotted so `{{tool_names.undo}}` brings them back{{/if}}.
- This is for plans, not source-code edits.
