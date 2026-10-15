#!/usr/bin/env bash
set -euo pipefail
shopt -s nullglob dotglob

fail() {
    printf 'error: example image resources: %s\n' "$*" >&2
    exit 1
}

usage() {
    printf 'usage: %s [--check] <new-build-context-directory>\n' "$0" >&2
    exit 1
}

# Deliberately do not copy the checkout or recursively copy arbitrary example
# files: local credentials and DMS configuration must never enter the bundle.
files=(Containerfile.niri Containerfile.smoke fedora-cocoa-way.repo
       cocoa-way-clipboard-relay cocoa-way-audio-relay)

check_context() {
    local context=$1 input name
    local entries=()
    [[ -d "$context" && ! -L "$context" ]] || fail "missing context: $context"
    entries=("$context"/*)
    [[ ${#entries[@]} -eq 1 && "${entries[0]}" == "$context/examples" ]] \
        || fail "unexpected or missing files in $context"
    [[ -d "$context/examples" && ! -L "$context/examples" ]] || fail "invalid examples directory"
    entries=("$context/examples"/*)
    [[ ${#entries[@]} -eq 1 && "${entries[0]}" == "$context/examples/container-images" ]] \
        || fail "unexpected or missing example directories"
    input="$context/examples/container-images"
    [[ -d "$input" && ! -L "$input" ]] || fail "invalid container-images directory"
    entries=("$input"/*)
    [[ ${#entries[@]} -eq ${#files[@]} ]] || fail "unexpected or missing files in $input"
    for name in "${files[@]}"; do
        [[ -f "$input/$name" && -s "$input/$name" && -r "$input/$name" && ! -L "$input/$name" ]] \
            || fail "missing, empty, unreadable or symlinked input: $input/$name"
        case "$name" in
            cocoa-way-*-relay) [[ -x "$input/$name" ]] || fail "relay is not executable: $input/$name" ;;
        esac
    done
}

check_only=false
if [[ "${1:-}" == --check ]]; then
    check_only=true
    shift
fi
[[ $# -eq 1 && -n "$1" ]] || usage
destination=${1%/}
[[ -n "$destination" ]] || usage
if $check_only; then
    check_context "$destination"
    printf 'Verified example image context: %s\n' "$destination"
    exit 0
fi

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
source_dir="$script_dir/../examples/container-images"
for name in "${files[@]}"; do
    [[ -f "$source_dir/$name" && -s "$source_dir/$name" && -r "$source_dir/$name" && ! -L "$source_dir/$name" ]] \
        || fail "missing, empty, unreadable or symlinked source: $source_dir/$name"
done
# Refuse merges so stale or private files cannot survive a previous package run.
[[ ! -e "$destination" && ! -L "$destination" ]] || fail "destination already exists: $destination"
mkdir -p -- "$(dirname -- "$destination")"
mkdir -- "$destination"
trap 'rm -rf -- "$destination"' EXIT
chmod 0755 "$destination"
install -d -m 0755 "$destination/examples" "$destination/examples/container-images"
for name in "${files[@]}"; do
    mode=0644
    case "$name" in cocoa-way-*-relay) mode=0755 ;; esac
    install -m "$mode" "$source_dir/$name" "$destination/examples/container-images/$name"
done
check_context "$destination"
trap - EXIT
printf 'Bundled example image context: %s\n' "$destination"
