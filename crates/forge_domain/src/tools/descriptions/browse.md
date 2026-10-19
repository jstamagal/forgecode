Read a web page the way a browser sees it, JavaScript included, through obscura (a self-contained browser engine). Pages that build themselves in JavaScript come back rendered; rendered dumps use a five-second settle window.
- `dump` (default `markdown`): `markdown` (links made absolute against the page URL), `text`, `html` (the DOM after scripts ran), `links` (URL, tab, anchor text per line), `original` (the raw response body as text), `assets` (one JSON object per sub-resource), `cookies` (the cookie jar as JSON).
- `screenshot`: a path to save a PNG of the settled page; the result is then only the `SCREENSHOT path=… size=…` line, so call again without it to read the page.
- A status of 400 or above leads the result with `ERROR: HTTP <code> from <url>` followed by the page.
- Private, loopback and link-local IP literals are allowed. No POST, headers or body.
{{#if tool_names.fetch}}
- Use `{{tool_names.fetch}}` for API calls that need a method, headers or a body.
{{/if}}
- obscura comes from forge's managed binaries (`forge tools --install`) or, failing that, PATH.
