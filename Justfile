default: dev

dev *args="":
    cargo build --release -p tauqe-server
    TAUQE_DEV_OPENAI_CACHE=1 cargo run --release -p tauqe-tui -- {{args}}

install:
    cargo install --path crates/server --locked
    cargo install --path crates/tui --locked
    @echo "Tauqe installed successfully in release mode! The 'tauqe' command is available."

[group('tauqe')]
server-logs *args="":
    lnav -I .lnav {{args}} .tauqe/server.log

logs-live:
    lnav -I .lnav -c ':goto 100%' .tauqe/server.log

docs-build:
    mdbook build docs

docs-serve *args="":
    mdbook serve docs {{args}}

loc *args="crates":
    tokei {{args}}

loc-no-tests:
    #!/usr/bin/env bash
    set -euo pipefail
    tmp=$(mktemp -d)
    trap 'rm -rf "$tmp"' EXIT
    cp -r crates "$tmp/"
    find "$tmp" -type d -name "tests" -exec rm -rf {} +
    find "$tmp" -type f \( -name "*_test.rs" -o -name "*_tests.rs" -o -name "tests.rs" \) -delete
    find "$tmp" -type f -name "*.rs" -exec sed -i -E '/^[[:space:]]*#\[(cfg\(test\)|test|tokio::test)\]/,$d' {} +
    (cd "$tmp" && tokei crates)

[group('tauqe')]
[doc('Verify the change set with Clippy, tests, doctests, and a rustdoc build')]
[metadata('timeout', '1200')]
verify:
    rust-script scripts/verify.rs

[group('tauqe')]
[doc('Rewrite source read from stdin onto stdout')]
[metadata('timeout', '120')]
format:
    @rustfmt --emit stdout

[group('tauqe')]
[doc('Compile every workspace target')]
[metadata('timeout', '600')]
check *args="--workspace":
    cargo check {{args}} --all-targets

[group('tauqe')]
[doc('Run the workspace test suite')]
[metadata('timeout', '900')]
test *args="--workspace":
    just test-binaries {{args}}
    just test-doc

[group('tauqe')]
[doc('Run a short OpenAI cache probe using two identical ChatGPT plan requests')]
[metadata('timeout', '600')]
openai-cache-probe:
    cargo test -p tauqe-core --lib providers::openai::cache_live_probe::openai_cache_probe -- --exact --ignored --nocapture

test-binaries *args="--workspace":
    cargo nextest run {{args}} --no-fail-fast --retries 0

test-doc *args="-p tauqe-flow -p tauqe-core -p tauqe-protocol -p tauqe-tools":
    cargo test --doc {{args}}

rustdoc *args="--workspace":
    cargo doc --no-deps {{args}}

[group('tauqe')]
[doc('Lint every workspace target, denying warnings')]
[metadata('timeout', '600')]
clippy *args="--workspace":
    # Lint normal builds before test targets enable test-support via dev-dependencies.
    cargo clippy {{args}} -- -D warnings
    cargo clippy {{args}} --all-targets -- -D warnings

[group('tauqe')]
[doc('Print the last N lines of the server journal')]
[metadata('timeout', '30')]
log-tail n="50":
    tail -n {{n}} .tauqe/server.log

[group('tauqe')]
[doc('Check for dead code across the workspace with cargo hawk')]
[metadata('timeout', '600')]
hawk *args="":
    cargo +1.99.0 hawk check {{args}}

[group('tauqe')]
[doc('Fix dead code across the workspace with cargo hawk')]
[metadata('timeout', '600')]
hawk-fix *args="":
    cargo +1.99.0 hawk check --fix {{args}}
