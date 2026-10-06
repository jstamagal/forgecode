Functions that outlive the session: save a function to disk, load saved functions into the kernel, keep every revision with its provenance.
- `action`: `list` (default; every saved function with its latest revision and scope), `show` (source of the latest or a given `revision`), `save` (`name` plus `source`: one top-level `def name(...)`; `note` says what changed), `load` (execs every saved function, or just `name`, into the kernel), `history` (every revision of `name` with date, session and note), `delete` (removes `name`; its revisions stay in the history file).
- `scope`: `global` (default, every project) or `project` (`.forge/toolbox/` in the working directory; shadows a global function of the same name).
- `save` never overwrites: each save is a new revision (`r1`, `r2`, …). It refuses a function that reads a free name the source does not define or import (a session variable, an unsaved helper); the error names every one. Imports used by the function go inside its body or above it in `source`.
{{#if tool_names.kernel}}
- Until `load` has run, the `{{tool_names.kernel}}` namespace has none of the saved functions.
{{/if}}
