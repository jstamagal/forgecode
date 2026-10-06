Version-current library documentation via Context7 (`CONTEXT7_API_KEY` optional). Use it before writing code against a third-party library, framework, SDK or API. It resolves `library` to Context7's best match and returns documentation snippets with source URLs. Context7 matches keywords, so a hit is reported only when the requested name (case and separators ignored) is the hit's project name (an id path segment or its title) or one whole word of it; anything else comes back as no match rather than an unrelated project's docs. Set `topic`; an untargeted dump is long.
{{#if tool_names.serper_search}}
`{{tool_names.serper_search}}` is for issues, changelogs and discussion around a library.
{{/if}}
