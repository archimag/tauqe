#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

if ! command -v rust-script >/dev/null 2>&1; then
    echo "verify: 'rust-script' is not installed. Install it via 'cargo install rust-script'" >&2
    exit 1
fi

exec rust-script scripts/verify.rs "$@"
