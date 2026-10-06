Web search via Tavily (needs `TAVILY_API_KEY`). Each result carries extracted page text, usually with a one-paragraph synthesized answer first, so research and "how do I" questions get read without fetching each page.
{{#if tool_names.serper_search}}
`{{tool_names.serper_search}}` is for exact phrases and the freshest news.
{{/if}}
{{#if tool_names.exa_search}}
`{{tool_names.exa_search}}` finds things you can describe but not name.
{{/if}}
{{#if tool_names.docs_search}}
`{{tool_names.docs_search}}` is for library documentation.
{{/if}}
