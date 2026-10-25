#!/usr/bin/env bash
#
# Change-aware verification dispatcher.
#
# The harness is the single owner of a turn's authoritative change set — the
# model's edits, the post-hook artifacts, and the files a repair round touched —
# and delivers it on standard input, one repository-relative path per line. This
# script decides which declared checks that change set requires, so the engine
# never learns which file names belong to which technology.
#
# The recipes it composes live in `Justfile`:
#   verify-cargo   compilation, lints, and tests of the workspace or target packages
#   verify-md      markdown hygiene of the changed documentation
#
# Neither recipe carries `[group('tauqe')]`, so the model never receives a second
# verification channel beside the mandatory lifecycle this script fills.
#
# The gate is never weakened: an empty change set, a terminal on standard input
# (a manual `just verify`), or a path whose suffix no rule recognizes selects the
# complete suite instead of a narrower one.

set -euo pipefail

# The recipes and the filter program are repository-relative, so the script
# anchors itself at the repository root whatever directory the caller used.
cd "$(dirname "${BASH_SOURCE[0]}")/.."

# The change set arrives on standard input. A terminal means the script was
# started by hand, and nothing piped in means the harness did not narrow the run:
# both cases fall through to the complete suite.
changed=()
if [ ! -t 0 ]; then
    while IFS= read -r path; do
        if [ -n "$path" ]; then
            changed+=("$path")
        fi
    done
fi

is_structured_protocol() {
    local proto="${TAUQE_PROTOCOL:-}"
    if [ -z "$proto" ] && [ -f "tauqe.toml" ]; then
        proto=$(sed -n -E 's/^[[:space:]]*protocol[[:space:]]*=[[:space:]]*"([^"]+)".*/\1/p' tauqe.toml | head -n 1)
    fi
    if [ -z "$proto" ]; then
        proto="structured"
    fi
    [ "$proto" = "structured" ]
}

has_tui=0
has_server=0
has_core=0
has_protocol=0
has_other_rs=0
has_root=0
has_md=0
unknown=0
markdown=()

if [ "${#changed[@]}" -ne 0 ]; then
    for path in "${changed[@]}"; do
        base=${path##*/}
        case "$path" in
            crates/tui/*)
                case "$path" in
                    *.rs|*/Cargo.toml) has_tui=1 ;;
                    *.md) has_md=1; markdown+=("$path") ;;
                    *) unknown=1 ;;
                esac
                ;;
            crates/server/*)
                case "$path" in
                    *.rs|*/Cargo.toml) has_server=1 ;;
                    *.md) has_md=1; markdown+=("$path") ;;
                    *) unknown=1 ;;
                esac
                ;;
            crates/core/*)
                case "$path" in
                    *.rs|*/Cargo.toml) has_core=1 ;;
                    *.md) has_md=1; markdown+=("$path") ;;
                    *) unknown=1 ;;
                esac
                ;;
            crates/protocol/*)
                case "$path" in
                    *.rs|*/Cargo.toml) has_protocol=1 ;;
                    *.md) has_md=1; markdown+=("$path") ;;
                    *) unknown=1 ;;
                esac
                ;;
            *.md)
                has_md=1
                markdown+=("$path")
                ;;
            *.rs)
                has_other_rs=1
                ;;
            *)
                case "$base" in
                    Cargo.toml|Cargo.lock|rustfmt.toml|Justfile|tauqe.toml)
                        has_root=1
                        ;;
                    *)
                        case "$path" in
                            scripts/*) has_root=1 ;;
                            *) unknown=1 ;;
                        esac
                        ;;
                esac
                ;;
        esac
    done
fi

run_cargo=0
cargo_args=()
run_md=0

# Markdown verification is skipped entirely in structured protocol mode,
# as XML turn-marker tag leakage only exists in XML-based protocols.
if ! is_structured_protocol; then
    if [ "${#changed[@]}" -eq 0 ] || [ "$unknown" -eq 1 ] || [ "$has_md" -eq 1 ]; then
        run_md=1
    fi
fi

if [ "${#changed[@]}" -eq 0 ] || [ "$unknown" -eq 1 ] || [ "$has_protocol" -eq 1 ] || [ "$has_root" -eq 1 ] || [ "$has_other_rs" -eq 1 ]; then
    run_cargo=1
    cargo_args=("--workspace")
else
    if [ "$has_core" -eq 1 ]; then
        cargo_args+=("-p" "tauqe-core" "-p" "tauqe-server")
    fi
    if [ "$has_server" -eq 1 ]; then
        already_has_server=0
        for ((i=0; i<${#cargo_args[@]}; i+=2)); do
            if [ "${cargo_args[i+1]}" = "tauqe-server" ]; then
                already_has_server=1
                break
            fi
        done
        if [ "$already_has_server" -eq 0 ]; then
            cargo_args+=("-p" "tauqe-server")
        fi
    fi
    if [ "$has_tui" -eq 1 ]; then
        cargo_args+=("-p" "tauqe-tui")
    fi

    if [ "${#cargo_args[@]}" -gt 0 ]; then
        run_cargo=1
    fi
fi

printf 'verify: %d changed path(s); cargo=%d (%s) markdown=%d\n' \
    "${#changed[@]}" "$run_cargo" "${cargo_args[*]:-none}" "$run_md" >&2

# The first failing check decides the verdict, so the remaining ones do not run
# against a change set that already failed.
status=0
if [ "$run_cargo" -eq 1 ]; then
    just verify-cargo "${cargo_args[@]}" || status=$?
fi

# The recipes run without `--quiet`: `just --quiet` silences the recipe's own
# output, not just the echoed command line, and the `path:line: error:`
# records must reach the harness's diagnostic stream. The recipe lines carry
# their own `@` prefixes, so no echo ever pollutes stdout.
if [ "$status" -eq 0 ] && [ "$run_md" -eq 1 ]; then
    if [ "${#markdown[@]}" -gt 0 ]; then
        just verify-md "${markdown[@]}" || status=$?
    else
        just verify-md || status=$?
    fi
fi

exit "$status"
