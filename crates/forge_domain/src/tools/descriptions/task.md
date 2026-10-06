Launch one or more worker agents for complex, multi-step work. Each worker runs autonomously in its own conversation and returns one result to you; the result is not shown to the user, so summarize it in your reply.

Available agents and their tools:
{{#each agents}}
- **{{id}}**{{#if description}}: {{description}}{{/if}}{{#if tools}}
  - Tools: {{#each tools}}{{this}}{{#unless @last}}, {{/unless}}{{/each}}{{/if}}
{{/each}}

When to use: open-ended searches that need several search/read rounds, independent investigations that can run concurrently, work that suits another agent's persona and tools.
When not to use: reading one known file or two or three files (use `{{tool_names.read}}`), searching for an exact identifier or definition (use `{{tool_names.fs_search}}`), anything one or two direct tool calls finish.

Inputs:
- `tasks` (required to start workers): one self-contained prompt per worker. A fresh worker cannot see this conversation; include paths, error text, constraints, the expected output shape, and whether it may change code.
- `agent_id` (required to start workers): the agent the workers run as. An unknown agent is an error and no worker starts.
- `session_id`: resume a worker session (the id is named in an earlier result as `session_id=…`). It runs exactly one task; an unknown id is an error.
- `model`: run the workers on another model. Leave it unset unless the user asks for it.

Parallelism: the tasks of one call run concurrently through a bounded worker pool, and several `{{tool_names.task}}` calls in one response also run concurrently. One task returns the worker's reply verbatim; several come back numbered under a `TASK_RESULTS` header in task order. A worker that fails reports its error in its own slot without discarding the others. Each reply is held to an equal share of the result budget. Workers that start their own tasks run one level deeper; past the configured depth limit a call is refused.

Background: a call blocks until every worker finishes. With `background=true` it returns at once with a handle while the workers keep running:

    task running in the background (handle t1): ...
    HANDLE t1 RUNNING

`action="poll", handle="t1"` says whether it is still running and returns the result once done; `action="wait", handle="t1", timeout=N` blocks up to N seconds, or until the end without `timeout`; `action="kill", handle="t1"` stops it. `handle` defaults to your most recent running task, and you see only tasks this conversation started. The finished result is also written to the file the handle names, and if you have not polled it by your next turn, a reminder names that file. Use background runs for slow, independent work you do not need before your next step.
