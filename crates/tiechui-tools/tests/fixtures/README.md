# Behavioral fixtures

- `bash-spec.json`: the bash tool's `name`, `description`, and `input_schema`.
- `truncation.json`: 728 head and tail truncation cases, combining empty text,
  final newlines, blank lines, CRLF, BOM, and multibyte characters with byte
  limits 1, 2, 3, 4, 7, 12, 51200 and line limits 1, 2, 3, 2000.
- `bash-sanitization.json`: binary output sanitization applied to every C0
  control, DEL, a C1 control, U+FFF9–U+FFFB, and an emoji. Bash updates and
  result text use this filter; spill files retain raw bytes.

The separate registry snapshot asserts the serialized tool specification bytes,
including deterministic registry and JSON object ordering.
