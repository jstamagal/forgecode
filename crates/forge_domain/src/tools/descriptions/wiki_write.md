Create or replace a structured wiki page with front matter.
- `kind` selects the page family: `source`, `entity`, `concept` or `synthesis`; the page goes to `<vault>/<kind>s/<slug>.md`.
- `vault` must name an existing vault directory; a blank or `.` vault is refused.
- An existing page is protected unless `overwrite=true`; pass a fully merged body when replacing.
- Tags, type and update date are normalized by the tool. Use wikilinks in `body` where knowledge should connect.
