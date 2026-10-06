Run one command with `{{env.shell}}` and return its exit code, stdout and stderr. Under bash and zsh a pipeline fails when any stage fails (`pipefail`), so `false | cat` reports exit 1. The working directory is `cwd`, default `{{env.cwd}}`; set `cwd` instead of using `cd`.

Long output comes back as its head and its tail, with a marker between them naming the bytes left out and the file that holds the whole stream:

    [truncated: output budget (… bytes) reached; stdout bytes 25000-980000 of 1004000 (955000 bytes) are not shown; the whole stdout is at /path/shell-3-….log — read it on with range {"start_byte": 25000}]

Read any part of that file with `{{tool_names.read}}` and a byte range, so do not pipe through `head` or `tail` just to shrink the output. Stdin is `/dev/null` and there is no terminal: a command that reads input gets end-of-file, and a password or confirmation prompt gets no answer.

A command does not have to finish before you get an answer. The call waits `timeout` seconds (default 30); a command that finishes inside that window comes back whole, and one still running comes back as a handle with whatever it has printed so far:

    command still running after 30s (handle 3, pid 4242). ...
    HANDLE 3 RUNNING

The command is never killed by the wait. Then `action="poll", handle="3"` returns new output and whether it is still running, `action="wait", handle="3", timeout=N` blocks up to N more seconds, and `action="kill", handle="3"` stops it (SIGTERM, then SIGKILL, to its whole process group). `handle` defaults to the most recent running job. Do a poll before assuming a long build or test run has failed. Interrupting the turn kills a command the call is still waiting on.

The command gets a filtered environment: the configured allow-list (by default PATH, HOME, USER, LANG, LC_ALL, TERM, PWD, SHELL) plus the names you list in `env`. When the command references a variable that was filtered out, the result says so.

- Independent commands can go in parallel calls in one response; chain dependent ones with `&&` in one call.
- Search with `{{tool_names.fs_search}}`, not `grep`, `rg`, or `find`.
- Read files with `{{tool_names.read}}`, not `cat`, `head`, or `tail`.
- Edit files with `{{tool_names.patch}}`, not `sed` or `awk`.
- Write files with `{{tool_names.write}}`, not redirects or heredocs.
- Delete with `{{tool_names.remove}}`, not `rm`.
