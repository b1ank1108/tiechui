# tiechui-cli

The `chui` and `tiechui` command-line entry points share a current-thread Tokio
runtime. See the [root README](../../README.md) for configuration and usage.

## Windows MSVC stack

`build.rs` reserves 8 MiB for each CLI binary's main-thread stack with
`/STACK:8388608`. It checks Cargo's target configuration, not the build host, and
emits a binary-only linker argument. Libraries, integration-test executables,
Windows GNU and Unix targets do not receive this setting. The linker retains its
default initial stack commit size.

This provides headroom for polling nested session futures on the main thread;
large search buffers remain heap-allocated. The CLI regression test reads the PE
headers of both binaries to verify their stack reserve.
