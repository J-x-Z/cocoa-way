#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 1 ]]; then
    echo "usage: $0 <path-to-app-bundle>" >&2
    exit 1
fi

APP_DIR=$1
MAIN_BIN="${APP_DIR}/Contents/MacOS/cocoa-way"
FRAMEWORKS_DIR="${APP_DIR}/Contents/Frameworks"
RESOURCES_DIR="${APP_DIR}/Contents/Resources"

if [[ ! -f "${MAIN_BIN}" ]]; then
    echo "error: missing app executable at ${MAIN_BIN}" >&2
    exit 1
fi

mkdir -p "${FRAMEWORKS_DIR}" "${RESOURCES_DIR}"

find_xkb_config_root() {
    local candidate
    local pkg_config_root=""

    if command -v pkg-config >/dev/null 2>&1; then
        pkg_config_root=$(pkg-config --variable=xkb_base xkeyboard-config 2>/dev/null || true)
    fi

    for candidate in \
        "${XKB_CONFIG_ROOT:-}" \
        "${pkg_config_root}" \
        /opt/homebrew/share/X11/xkb \
        /usr/local/share/X11/xkb \
        /usr/share/X11/xkb; do
        if [[ -n "${candidate}" && -f "${candidate}/rules/evdev" ]]; then
            printf '%s\n' "${candidate}"
            return 0
        fi
    done

    return 1
}

XKB_ROOT=$(find_xkb_config_root || true)
if [[ -z "${XKB_ROOT}" ]]; then
    echo "error: xkeyboard-config data was not found" >&2
    exit 1
fi

rm -rf "${RESOURCES_DIR}/xkb"
mkdir -p "${RESOURCES_DIR}/xkb"
cp -R "${XKB_ROOT}/." "${RESOURCES_DIR}/xkb/"
if [[ ! -f "${RESOURCES_DIR}/xkb/rules/evdev" ]]; then
    echo "error: failed to bundle xkeyboard-config rules" >&2
    exit 1
fi

APP_EXECUTABLES=("${MAIN_BIN}")
for helper in cocoa-wayctl cocoa-way-mcp; do
    helper_path="${APP_DIR}/Contents/MacOS/${helper}"
    if [[ -f "${helper_path}" ]]; then
        APP_EXECUTABLES+=("${helper_path}")
    fi
done

QUEUE=("${APP_EXECUTABLES[@]}")

is_app_executable() {
    local candidate=$1
    local executable

    for executable in "${APP_EXECUTABLES[@]}"; do
        if [[ "${candidate}" == "${executable}" ]]; then
            return 0
        fi
    done

    return 1
}

is_bundle_candidate() {
    case "$1" in
        /System/*|/usr/lib/*|@rpath/*|@loader_path/*|@executable_path/*)
            return 1
            ;;
        *)
            return 0
            ;;
    esac
}

add_rpath_if_needed() {
    local file=$1
    local rpath=$2

    install_name_tool -add_rpath "${rpath}" "${file}" 2>/dev/null || true
}

queue_file() {
    local file=$1
    local queued

    for queued in "${QUEUE[@]}"; do
        if [[ "${queued}" == "${file}" ]]; then
            return
        fi
    done

    QUEUE+=("${file}")
}

copy_and_rewrite_dependency() {
    local current=$1
    local dependency=$2
    local basename
    local bundled_path
    local rewritten_path

    basename=$(basename "${dependency}")
    bundled_path="${FRAMEWORKS_DIR}/${basename}"

    if [[ ! -e "${bundled_path}" ]]; then
        cp "${dependency}" "${bundled_path}"
        chmod u+w "${bundled_path}"
        install_name_tool -id "@rpath/${basename}" "${bundled_path}"
        add_rpath_if_needed "${bundled_path}" "@loader_path"
        queue_file "${bundled_path}"
    fi

    if is_app_executable "${current}"; then
        rewritten_path="@executable_path/../Frameworks/${basename}"
    else
        rewritten_path="@rpath/${basename}"
        add_rpath_if_needed "${current}" "@loader_path"
    fi

    install_name_tool -change "${dependency}" "${rewritten_path}" "${current}" 2>/dev/null || true
}

for executable in "${APP_EXECUTABLES[@]}"; do
    add_rpath_if_needed "${executable}" "@executable_path/../Frameworks"
done

index=0
while [[ ${index} -lt ${#QUEUE[@]} ]]; do
    current=${QUEUE[${index}]}
    index=$((index + 1))

    while IFS= read -r dependency; do
        [[ -n "${dependency}" ]] || continue

        if is_bundle_candidate "${dependency}"; then
            copy_and_rewrite_dependency "${current}" "${dependency}"
        fi
    done < <(otool -L "${current}" | tail -n +2 | awk '{print $1}')
done

# Homebrew bottles can require a newer macOS than the Rust deployment target.
# Declare the strictest bundled requirement before signing the app.
version_number() {
    local major minor patch
    IFS=. read -r major minor patch <<< "$1"
    printf '%s\n' "$((10#${major:-0} * 1000000 + 10#${minor:-0} * 1000 + 10#${patch:-0}))"
}

MIN_MACOS=$(/usr/libexec/PlistBuddy -c 'Print :LSMinimumSystemVersion' "${APP_DIR}/Contents/Info.plist")
for current in "${QUEUE[@]}"; do
    while IFS= read -r required; do
        [[ -n "${required}" ]] || continue
        if [[ $(version_number "${required}") -gt $(version_number "${MIN_MACOS}") ]]; then
            MIN_MACOS=${required}
        fi
    done < <(otool -l "${current}" | awk '
        $1 == "cmd" { minimum = ($2 == "LC_BUILD_VERSION" || $2 == "LC_VERSION_MIN_MACOSX") }
        minimum && ($1 == "minos" || $1 == "version") { print $2; minimum = 0 }
    ')
done
/usr/libexec/PlistBuddy -c "Set :LSMinimumSystemVersion ${MIN_MACOS}" "${APP_DIR}/Contents/Info.plist"
printf 'Bundled app requires macOS %s or later\n' "${MIN_MACOS}"

# install_name_tool invalidates the ad-hoc signatures Cargo places on Mach-O
# binaries. Re-sign nested code first so macOS can launch the bundled helpers.
if command -v codesign >/dev/null 2>&1; then
    while IFS= read -r bundled_library; do
        codesign --force --sign - --timestamp=none "${bundled_library}"
    done < <(find "${FRAMEWORKS_DIR}" -type f -print)

    for executable in "${APP_EXECUTABLES[@]}"; do
        codesign --force --sign - --timestamp=none "${executable}"
    done

    codesign --force --deep --sign - --timestamp=none "${APP_DIR}"
fi
