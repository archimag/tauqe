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
#   verify-cargo   compilation, lints, and tests of the workspace
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

run_cargo=0
run_md=0
unknown=0
markdown=()

if [ "${#changed[@]}" -ne 0 ]; then
    for path in "${changed[@]}"; do
        base=${path##*/}
        case "$path" in
            *.md)
                run_md=1
                markdown+=("$path")
                ;;
            *.rs)
                run_cargo=1
                ;;
            *)
                # A workspace manifest invalidates every compiled artifact, so
                # it selects the same checks as the sources it describes.
                case "$base" in
                    Cargo.toml|Cargo.lock|rustfmt.toml)
                        run_cargo=1
                        ;;
                    *)
                        unknown=1
                        ;;
                esac
                ;;
        esac
    done
fi

if [ "${#changed[@]}" -eq 0 ] || [ "$unknown" -eq 1 ]; then
    run_cargo=1
    run_md=1
fi

printf 'verify: %d changed path(s); cargo=%d markdown=%d\n' \
    "${#changed[@]}" "$run_cargo" "$run_md" >&2

# The first failing check decides the verdict, so the remaining ones do not run
# against a change set that already failed.
status=0
if [ "$run_cargo" -eq 1 ]; then
    just verify-cargo || status=$?
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
