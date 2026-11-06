fmt:
    cargo fmt --all -- --check

lint:
    cargo clippy --workspace --all-targets --all-features --locked -- -D warnings

test:
    cargo nextest run --workspace --all-features --locked

deny:
    cargo deny --locked --offline check

schema-check:
    cargo run --locked -p xtask -- schema --check

sdk-check:
    cargo run --locked -p xtask -- typescript --check
    @if command -v node >/dev/null 2>&1; then node --test sdk/typescript/tests/*.test.ts; else echo 'SKIP TypeScript SDK tests: node is not installed'; fi

plugins-check:
    @if [ -d ../tiechui-plugins ]; then cargo build --locked -p tiechui-cli --bin tc && python3 -B scripts/plugins.py check; else echo 'SKIP plugins: sibling tiechui-plugins repository is absent'; fi

pty *cases:
    cargo build --locked -p tiechui-cli --bin tc
    cargo build --locked -p tiechui-module-plugins --features test-server --bin t2-test-plugin
    python3 -B scripts/plugins.py build-rust
    python3 -B -m unittest discover -s tests/pty -p 'test_*.py'
    python3 -B tests/pty/smoke.py {{cases}}

packaging-check:
    python3 -B -m unittest discover -s tests/packaging -p 'test_*.py'

check: fmt lint test deny schema-check sdk-check packaging-check
