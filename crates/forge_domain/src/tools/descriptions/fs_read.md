Read one file: text, PDF, image, or notebook. Relative paths resolve against the working directory.
- Text comes back as `12:content`: line number, a colon, then the text. Lines are returned whole, however long they are. The prefix is not file content; strip it when you quote text back into an edit. `show_line_numbers: false` returns the text without prefixes.
- A read stops after {{config.maxReadSize}} lines and ends with a footer naming the exact `range` that continues it. Use `range` with `start_line`/`end_line` for a slice of the file.
- A whole-file read (no `range`) of a file larger than {{config.maxReadBytes}} bytes is refused with a suggested ranged call; ranged reads are not subject to that cap.
- `range` also takes a byte range, `start_byte` (0-based) and optional `end_byte` (one past the last byte), for text with few or no line breaks: it returns raw text without line prefixes, one page at a time, snapped to UTF-8 character boundaries, and names the offset to continue from. Use it to page through a saved result file.
- Pass either a line range or a byte range, never both.
- Reading the same lines of an unchanged file again, while the earlier result is still in the conversation, returns a short note naming the read call that showed them.
- A Jupyter notebook (.ipynb) read without a range comes back as its cells, each with its id, type, source and a summary of its outputs; a line or byte range reads its raw JSON.
- A PDF comes back as extracted text (via `pdftotext`).
{{#if (contains model.input_modalities "image")}}
- Images (PNG, JPEG, GIF, WebP) come back as images; a PDF with no extractable text comes back as an image. Images and image-fallback PDFs larger than {{config.maxImageSize}} bytes are refused.
{{/if}}
- Directories, pipes and other special files are refused{{#if tool_names.fs_search}}; use `{{tool_names.fs_search}}` to find files{{/if}}.
- Reading records which lines you have seen.{{#if tool_names.write}} An overwriting `{{tool_names.write}}` requires a prior read of the file.{{/if}}{{#if tool_names.patch}} `{{tool_names.patch}}` requires a prior read and that every line it changes was shown.{{/if}}
- You can call multiple tools in a single response; read several files in parallel when you need them.
