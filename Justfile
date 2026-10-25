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
    find "$tmp" -type f \( -name "*_test.rs" -o -name "*_tests.rs" -o -name "tests.rs" \) -delete
    find "$tmp" -type f -name "*.rs" -exec sed -i -E '/^[[:space:]]*#\[(cfg\(test\)|test|tokio::test)\]/,$d' {} +
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

# Verify the change set, dispatching to the checks its files require.
#
# The harness is the single owner of a turn's authoritative change set — the
# model's edits, the post-hook artifacts, the files a repair round touched — and
# delivers it on standard input, one repository-relative path per line.
# `scripts/verify.sh` reads that list and runs only the recipes it selects:
# `verify-cargo` for Rust sources and workspace manifests, `verify-md` for
# documentation. An empty list, a terminal on standard input (a manual
# `just verify`), or a path whose suffix no rule recognizes selects the complete
# suite, so a narrower change set never weakens the gate.
#
# The script never inspects Git: while the transaction is open the working tree
# also holds post-hook artifacts and the developer's uncommitted work, so it
# cannot assemble that list itself.
#
# Formatting is deliberately absent. `format` below is a constructive in-memory
# filter applied during Staging, never a correctness gate, so a style difference
# must not fail verification or start a healing cycle.
[group('tauqe')]
[doc('Verify the change set: compilation, lints, tests, and markdown hygiene')]
[metadata('timeout', '1200')]
verify:
    bash scripts/verify.sh

# Compile, lint, and test the workspace. Internal to `scripts/verify.sh`.
#
# The recipe carries no `[group('tauqe')]` attribute, so the harness never
# publishes it: a second verification channel beside the mandatory `verify`
# lifecycle would let the model choose a weaker gate. The recipes it reuses stay
# independently addressable as typed custom tools.
verify-cargo *args="--workspace":
    cargo check {{args}} --all-targets
    cargo clippy {{args}} --all-targets -- -D warnings
    cargo test {{args}}

# Report markdown hygiene findings among the paths it is given. Internal to
# `scripts/verify.sh`; the `check-md` tool reuses it for the whole corpus.
#
# The dispatcher supplies the markdown paths of the change set as arguments. The
# filter prints `path:line: error: message` records — the shape the harness
# parses into typed diagnostics — and exits non-zero when it reports one. A run
# without paths examines nothing, because a finding outside the change set
# describes the repository, not the turn, and would fail a gate the turn cannot
# repair.
verify-md *files:
    @awk -f scripts/mdcheck.awk {{files}}

# Rewrite source arriving on standard input onto standard output.
#
# This is the in-memory formatting filter: the harness feeds staged content to
# stdin and takes the rewritten content from stdout, so formatting happens before
# any byte reaches disk. The `@` prefix keeps stdout free of the echoed command
# line, so the stream carries source and nothing else.
[group('tauqe')]
[doc('Rewrite source read from stdin onto stdout')]
[metadata('timeout', '120')]
format:
    @rustfmt --emit stdout

# Compile every workspace target without running tests or lints
[group('tauqe')]
[doc('Compile every workspace target')]
[metadata('timeout', '600')]
check *args="--workspace":
    cargo check {{args}} --all-targets

# Run the workspace test suite
[group('tauqe')]
[doc('Run the workspace test suite')]
[metadata('timeout', '900')]
test *args="--workspace":
    cargo test {{args}}

# Lint every workspace target with warnings denied
[group('tauqe')]
[doc('Lint every workspace target, denying warnings')]
[metadata('timeout', '600')]
clippy *args="--workspace":
    cargo clippy {{args}} --all-targets -- -D warnings

# Check formatting of every workspace source file
[group('tauqe')]
[doc('Check formatting of every workspace source file')]
[metadata('timeout', '300')]
fmt:
    cargo fmt --all -- --check

# Check markdown hygiene of every tracked markdown file.
#
# The published counterpart of the internal `verify-md`: the dispatcher feeds
# `verify-md` with the markdown paths of the change set, while this tool
# addresses the whole tracked corpus, so documentation hygiene can be inspected
# on demand. It reports findings and fills no lifecycle slot: `verify` remains
# the mandatory gate, and a finding here describes the repository, not the
# turn.
#
# The nested call carries no `--quiet`: `just --quiet` silences the recipe's
# own output, not just the echoed command line, so every
# `path:line: error:` record would be discarded before it reached the tool
# feedback. The `@` prefixes alone keep the stream free of echoes.
[group('tauqe')]
[doc('Check markdown hygiene of every tracked markdown file')]
[metadata('timeout', '60')]
check-md:
    @just verify-md $(git ls-files -- '*.md')

# Print the last N lines of the server journal (defaults to 50)
[group('tauqe')]
[doc('Print the last N lines of the server journal')]
[metadata('timeout', '30')]
log-tail n="50":
    tail -n {{n}} .tauqe/server.log
