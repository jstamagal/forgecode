Semantic web search via Exa (needs `EXA_API_KEY`): describe what you want in a full sentence and it finds pages by meaning, each with a slice of page text. Best when you know what you mean but not what it is called.
{{#if tool_names.serper_search}}
`{{tool_names.serper_search}}` is for exact strings, names and error messages.
{{/if}}
{{#if tool_names.tavily_search}}
`{{tool_names.tavily_search}}` is for research questions that want a synthesized answer.
{{/if}}
{{#if tool_names.docs_search}}
`{{tool_names.docs_search}}` is for library documentation.
{{/if}}
