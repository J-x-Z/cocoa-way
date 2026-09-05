#!/bin/bash
set -euo pipefail

usage() {
    echo "Usage: $0 [--display SLOT] [waypipe arguments...]" >&2
    echo "       $0 --list-displays" >&2
    echo "Example: $0 ssh user@linux-host firefox" >&2
    echo "         $0 --display display-1 ssh user@linux-host niri" >&2
}

selected_slot="${COCOA_WAY_DISPLAY_SLOT:-}"
list_displays=false
while [ "$#" -gt 0 ]; do
    case "$1" in
        --display)
            if [ "$#" -lt 2 ] || [ -z "$2" ]; then
                echo "Error: --display requires a display slot name." >&2
                usage
                exit 2
            fi
            selected_slot="$2"
            shift 2
            ;;
        --list-displays)
            list_displays=true
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            break
            ;;
    esac
done

runtime_candidates=()
if [ -n "${COCOA_WAY_RUNTIME_DIR:-}" ]; then
    runtime_candidates+=("$COCOA_WAY_RUNTIME_DIR")
fi
if [ -n "${XDG_RUNTIME_DIR:-}" ]; then
    runtime_candidates+=("$XDG_RUNTIME_DIR")
fi
if [ -n "${TMPDIR:-}" ]; then
    runtime_candidates+=("${TMPDIR%/}/cocoa-way")
fi
runtime_candidates+=("/tmp/cocoa-way")

find_wayland_socket() {
    local directory="$1"
    [ -d "$directory" ] || return 1
    find "$directory" -maxdepth 1 -type s -name 'wayland-*' -print 2>/dev/null \
        | sort | head -n 1
}

find_default_display() {
    local candidate socket
    if [ -n "${XDG_RUNTIME_DIR:-}" ] && [ -n "${WAYLAND_DISPLAY:-}" ] && \
       [ -S "${XDG_RUNTIME_DIR%/}/$WAYLAND_DISPLAY" ]; then
        runtime_dir="${XDG_RUNTIME_DIR%/}"
        display="$WAYLAND_DISPLAY"
        return 0
    fi
    for candidate in "${runtime_candidates[@]}"; do
        socket=$(find_wayland_socket "$candidate" || true)
        if [ -n "$socket" ]; then
            runtime_dir="$candidate"
            display=$(basename "$socket")
            return 0
        fi
    done
    return 1
}

find_managed_display() {
    local requested_slot="$1"
    local marker candidate socket
    for marker in /tmp/cwd-*/display.slot; do
        [ -f "$marker" ] || continue
        managed_display_is_live "$marker" || continue
        [ "$(cat "$marker" 2>/dev/null || true)" = "$requested_slot" ] || continue
        candidate=$(dirname "$marker")
        socket=$(find_wayland_socket "$candidate" || true)
        if [ -n "$socket" ]; then
            runtime_dir="$candidate"
            display=$(basename "$socket")
            return 0
        fi
    done
    return 1
}

managed_display_is_live() {
    local marker="$1"
    local directory base parent_pid worker_pid command
    directory=$(dirname "$marker")
    if [ -f "$directory/display.parent" ]; then
        parent_pid=$(cat "$directory/display.parent" 2>/dev/null || true)
    else
        base=$(basename "$directory")
        parent_pid=${base#cwd-}
        parent_pid=${parent_pid%%-*}
    fi
    [[ "$parent_pid" =~ ^[0-9]+$ ]] || return 1
    kill -0 "$parent_pid" 2>/dev/null || return 1
    command=$(ps -p "$parent_pid" -o command= 2>/dev/null || true)
    [[ "$command" == *cocoa-way* ]] || return 1
    # Workers created before the PID marker was introduced remain discoverable
    # until Cocoa-Way restarts; new workers receive the stricter liveness check.
    [ -f "$directory/display.worker" ] || return 0
    worker_pid=$(cat "$directory/display.worker" 2>/dev/null || true)
    [[ "$worker_pid" =~ ^[0-9]+$ ]] || return 1
    kill -0 "$worker_pid" 2>/dev/null || return 1
    command=$(ps -p "$worker_pid" -o command= 2>/dev/null || true)
    [[ "$command" == *cocoa-way* ]]
}

runtime_dir=""
display=""

if $list_displays; then
    printf '%-18s %s\n' "SLOT" "SOCKET"
    if find_default_display; then
        printf '%-18s %s/%s\n' "default" "$runtime_dir" "$display"
    fi
    for marker in /tmp/cwd-*/display.slot; do
        [ -f "$marker" ] || continue
        managed_display_is_live "$marker" || continue
        slot=$(cat "$marker" 2>/dev/null || true)
        candidate=$(dirname "$marker")
        socket=$(find_wayland_socket "$candidate" || true)
        if [ -n "$slot" ] && [ -n "$socket" ]; then
            printf '%-18s %s\n' "$slot" "$socket"
        fi
    done
    exit 0
fi

if [ -n "$selected_slot" ] && [ "$selected_slot" != "default" ]; then
    if ! find_managed_display "$selected_slot"; then
        echo "Error: Cocoa-Way display slot '$selected_slot' is not available." >&2
        echo "Run '$0 --list-displays' to inspect live display slots." >&2
        exit 1
    fi
else
    find_default_display || true
fi

if [ -z "$runtime_dir" ] || [ -z "$display" ]; then
    echo "Error: Could not find a live Cocoa-Way socket for the current user." >&2
    echo "Start Cocoa-Way first, or set COCOA_WAY_RUNTIME_DIR explicitly." >&2
    exit 1
fi

if [ "$#" -eq 0 ]; then
    usage
    exit 2
fi

waypipe_bin="${COCOA_WAY_WAYPIPE:-$(command -v waypipe || true)}"
if [ -z "$waypipe_bin" ]; then
    echo "Error: waypipe is not installed on this Mac or is not available in PATH." >&2
    echo "Install it with: brew install J-x-Z/tap/waypipe-darwin" >&2
    exit 1
fi

export XDG_RUNTIME_DIR="$runtime_dir"
export WAYLAND_DISPLAY="$display"

echo "Found Cocoa-Way socket: $XDG_RUNTIME_DIR/$WAYLAND_DISPLAY"
if [ -n "$selected_slot" ]; then
    echo "Using Cocoa-Way display slot: $selected_slot"
fi
echo "Using waypipe: $waypipe_bin"

# If the command is 'ssh', automatically add the recommended StreamLocalBindUnlink=yes option
# This fixes "remote port forwarding failed" errors when the socket file already exists on the remote
if [ "$1" = "ssh" ]; then
    echo "Info: Detected SSH mode. Injecting '-o StreamLocalBindUnlink=yes' to fix socket conflicts."
    echo "Info: The remote Linux host must also have a compatible 'waypipe' command in PATH."
    shift
    exec "$waypipe_bin" --compress="${COCOA_WAY_COMPRESS:-zstd}" ssh \
        -o StreamLocalBindUnlink=yes "$@"
else
    exec "$waypipe_bin" "$@"
fi
