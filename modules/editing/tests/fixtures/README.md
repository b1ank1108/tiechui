# File tool goldens

`golden.json` holds checked-in expected outputs for the file tools. Run them with:

```sh
cargo test -p tiechui-module-editing --test contract
```

The test needs `rg` on `PATH` or in `$TIECHUI_HOME/bin`. File discovery is native.

`files` holds the fixed input bytes. Each entry in `cases` records the tool input
and either its error or its content/details/structuredContent, plus the file's
bytes after a successful mutation. `before` resets a case's input and
observations. The test recreates the tree and compares every string byte for
byte, every detail and structured value, and all recorded written bytes. Object
key order is ignored. Image cases cover raw PNG, BMP conversion, and default
resizing. The fixture registry disables image auto-resize;
`autoResizeImages: true` marks cases that enable it.

Tool descriptions and schemas live in `src/specs.json`, not in the fixture.
