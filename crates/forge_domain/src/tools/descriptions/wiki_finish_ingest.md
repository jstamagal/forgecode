Archive an ingested unit, append the log entry and commit the vault when possible.
- `source` is the ingested file under `<vault>/raw/`; it moves to `<vault>/raw/archive/`. Archive runs first; if it fails, no log entry or commit is created.
- The log entry (date, source, `note`) is appended to `<vault>/log.md`.
- The commit runs only when the vault is the root of its own git work tree and there is a diff; a vault nested inside another repository is not committed. A failed commit is an error, with the archive and log left in place to retry.
