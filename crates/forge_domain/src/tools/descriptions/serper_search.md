Google search via Serper (needs `SERPER_API_KEY`). Keyword queries: exact phrases, names, error messages, recent news. Returns Google's organic results as title, URL and snippet, with the answer box first when there is one.
{{#if tool_names.tavily_search}}
`{{tool_names.tavily_search}}` returns extracted page text and a synthesized answer instead of snippets.
{{/if}}
{{#if tool_names.exa_search}}
`{{tool_names.exa_search}}` finds things you can describe but not name.
{{/if}}
{{#if tool_names.docs_search}}
`{{tool_names.docs_search}}` is for library documentation.
{{/if}}
