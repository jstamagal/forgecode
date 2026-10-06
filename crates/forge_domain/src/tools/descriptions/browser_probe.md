Render and screenshot a page or local HTML file in real headless Chromium (Playwright; WebGL through SwiftShader, no GPU needed) and measure whether an interaction changes it.
- `click` is a `>`-separated chain of case-insensitive regexes matched against visible button or link text (e.g. `menu>settings`); `press` is a comma-separated list of keys pressed after the clicks.
- The result lists the PNG frames (after load, after the actions, and `hold_ms` later) with per-frame metrics: size, dominant-colour share, unique colours, changed pixels against the previous frame, WebGL availability, console and page errors. Metrics describe the frames; they do not judge them.
- Needs `python3` with the `playwright` package and its Chromium; a missing one is reported as an error.
{{#if tool_names.read}}
- `{{tool_names.read}}` a returned PNG to look at the frame.
{{/if}}
