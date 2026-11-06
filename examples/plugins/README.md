# Example plugins

The examples now live in the sibling [tiechui-plugins repository](../../../tiechui-plugins),
under `plugins/sdk-typescript` and `plugins/sdk-rust`. Their source is maintained
there; this checkout contains the SDKs and test harness.

Run `just plugins-check` to validate the marketplace, installed manifests,
vendored TypeScript SDK and TypeScript plugin tests. It skips when the sibling
repository is absent and is independent of `just check`.
`python3 -B scripts/plugins.py refresh-sdk` refreshes the vendored SDK.
`python3 -B scripts/plugins.py build-rust` builds a temporary copy of the Rust
example against this checkout's SDK into this checkout's `target/`.
