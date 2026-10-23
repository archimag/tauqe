# Default development environment
default: dev

# Build the server, then run the TUI for development
dev *args="":
    cargo build -p tauqe-server
    cargo run -p tauqe-tui -- {{args}}

# Install release builds of tauqe-server and tauqe into ~/.cargo/bin
# (`cargo install` always builds binaries in --release mode)
install:
    cargo install --path crates/server --locked
    cargo install --path crates/tui --locked
    @echo "Tauqe installed successfully in release mode! The 'tauqe' command is available."

# Interactive inspection and analysis of the server journal through lnav
logs *args="":
    lnav -I .lnav {{args}} .tauqe/server.log

# Live tail of the server journal in lnav (jumps to the end and keeps following)
logs-live:
    lnav -I .lnav -c ':goto 100%' .tauqe/server.log

# Build the documentation book with mdbook
docs-build:
    mdbook build docs

# Serve the documentation book locally with live reload (defaults to http://localhost:3000)
docs-serve *args="":
    mdbook serve docs {{args}}

# Count project lines of code (including tests) with tokei
loc *args="crates":
    tokei {{args}}

# Count project lines of code without tests (strip tests with find and sed)
loc-no-tests:
    #!/usr/bin/env bash
    set -euo pipefail
    tmp=$(mktemp -d)
    trap 'rm -rf "$tmp"' EXIT
    cp -r crates "$tmp/"
    find "$tmp" -type d -name "tests" -exec rm -rf {} +
    find "$tmp" -type f -name "*_test.rs" -delete
    find "$tmp" -type f -name "*.rs" -exec sed -i '/^[[:space:]]*#\[cfg(test)\]/,$d' {} +
    (cd "$tmp" && tokei crates)

# ---------------------------------------------------------------------------
# Harness command surface ([group('tauqe')])
#
# `tauqe.toml` declares `runner = "just"`, so the harness reads this justfile
# through `just --dump --dump-format json`. Only recipes carrying the
# `[group('tauqe')]` attribute are exported, so every administrative recipe
# above stays invisible to the model. `[doc(...)]` becomes the description of
# the published tool, and `[metadata('timeout', ...)]` sets its wall-clock
# budget in seconds. The `verify` recipe fills the verification lifecycle while
# the remaining recipes become typed custom tools. Declarations in `tauqe.toml`
# still take precedence over these recipes.
# ---------------------------------------------------------------------------

# Verify the workspace: formatting, compilation, lints, and the test suite
[group('tauqe')]
[doc('Verify the workspace: formatting, compilation, lints, and tests')]
[metadata('timeout', '1200')]
verify: fmt check clippy test

# Compile every workspace target without running tests or lints
[group('tauqe')]
[doc('Compile every workspace target')]
[metadata('timeout', '600')]
check:
    cargo check --all-targets

# Run the workspace test suite
[group('tauqe')]
[doc('Run the workspace test suite')]
[metadata('timeout', '900')]
test:
    cargo test --workspace

# Lint every workspace target with warnings denied
[group('tauqe')]
[doc('Lint every workspace target, denying warnings')]
[metadata('timeout', '600')]
clippy:
    cargo clippy --all-targets -- -D warnings

# Check formatting of every workspace source file
[group('tauqe')]
[doc('Check formatting of every workspace source file')]
[metadata('timeout', '300')]
fmt:
    cargo fmt --all -- --check

# Print the last N lines of the server journal (defaults to 50)
[group('tauqe')]
[doc('Print the last N lines of the server journal')]
[metadata('timeout', '30')]
log-tail n="50":
    tail -n {{n}} .tauqe/server.log
