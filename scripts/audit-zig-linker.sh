#!/bin/bash
# Zig does not accept rustc's GNU-ld-only Cortex-A53 workaround flag.
set -euo pipefail
args=()
for arg in "$@"; do
    case "$arg" in
        -Wl,--fix-cortex-a53-843419) ;;
        *) args+=("$arg") ;;
    esac
done
exec /opt/homebrew/bin/zig cc -target aarch64-linux-musl "${args[@]}"
