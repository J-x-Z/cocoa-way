use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

const XKB_CONFIG_ROOT_ENV: &str = "XKB_CONFIG_ROOT";

static APPLE_DATA_ROOT: LazyLock<Mutex<Option<PathBuf>>> = LazyLock::new(|| Mutex::new(None));

pub(crate) fn remember_apple_container_status(status: &serde_json::Value) {
    if let Some(path) = apple_data_root_from_status(status) {
        *APPLE_DATA_ROOT.lock().unwrap() = Some(path);
    }
}

pub(crate) fn apple_container_data_root() -> PathBuf {
    APPLE_DATA_ROOT.lock().unwrap().clone().unwrap_or_else(|| {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/Users"))
            .join("Library/Application Support/com.apple.container")
    })
}

fn apple_data_root_from_status(status: &serde_json::Value) -> Option<PathBuf> {
    status
        .pointer("/paths/appRoot")
        .or_else(|| status.get("appRoot"))
        .and_then(serde_json::Value::as_str)
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
}

pub(crate) fn configure_xkb_config_root() -> Result<PathBuf, String> {
    let configured = std::env::var_os(XKB_CONFIG_ROOT_ENV).map(PathBuf::from);
    if let Some(root) = configured
        .as_deref()
        .filter(|root| is_xkb_config_root(root))
    {
        log::info!("Using XKB configuration from {}", root.display());
        return Ok(root.to_path_buf());
    }

    let executable = std::env::current_exe()
        .map_err(|error| format!("failed to locate the Cocoa-Way executable: {error}"))?;
    let mut candidates = Vec::new();
    if let Some(root) = bundled_xkb_config_root_for(&executable) {
        candidates.push(root);
    }
    candidates.extend([
        PathBuf::from("/opt/homebrew/share/X11/xkb"),
        PathBuf::from("/usr/local/share/X11/xkb"),
        PathBuf::from("/usr/share/X11/xkb"),
        PathBuf::from("/run/current-system/sw/share/X11/xkb"),
        PathBuf::from("/nix/var/nix/profiles/default/share/X11/xkb"),
    ]);

    if let Some(root) = candidates.into_iter().find(|root| is_xkb_config_root(root)) {
        // SAFETY: this runs on the main thread before xkbcommon or child processes
        // are initialized.
        unsafe { std::env::set_var(XKB_CONFIG_ROOT_ENV, &root) };
        log::info!("Using XKB configuration from {}", root.display());
        return Ok(root);
    }

    let configured = configured
        .map(|path| format!(" The configured path '{}' is incomplete.", path.display()))
        .unwrap_or_default();
    Err(format!(
        "Cocoa-Way could not find xkeyboard-config data.{configured} Reinstall the app or install xkeyboard-config."
    ))
}

fn bundled_xkb_config_root_for(executable: &Path) -> Option<PathBuf> {
    let contents = executable.parent()?.parent()?;
    Some(contents.join("Resources/xkb"))
}

fn is_xkb_config_root(path: &Path) -> bool {
    path.join("rules/evdev").is_file()
        && path.join("keycodes/evdev").is_file()
        && path.join("symbols/us").is_file()
}

pub(crate) fn resolve_command_path(
    name: &str,
    configured: Option<&str>,
    display_name: &str,
    child_path: &str,
) -> Option<PathBuf> {
    if let Some(path) = configured.filter(|path| !path.trim().is_empty()) {
        let path = expand_home(path.trim());
        if is_executable_file(&path) {
            return Some(path);
        }

        log::error!(
            "Configured path for {} does not point to an executable file: {:?}",
            display_name,
            path
        );
        return None;
    }

    let mut searched = Vec::new();

    if let Some(path) = find_executable_in_path(name, &std::env::var_os("PATH"), &mut searched) {
        return Some(path);
    }

    if let Some(path) = find_executable_in_path(name, &Some(child_path.into()), &mut searched) {
        return Some(path);
    }

    log::error!(
        "Failed to find {}. Searched: {}.",
        display_name,
        searched
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );
    None
}

pub(crate) fn find_command_path(name: &str, child_path: &str) -> Option<PathBuf> {
    let mut searched = Vec::new();

    find_executable_in_path(name, &std::env::var_os("PATH"), &mut searched)
        .or_else(|| find_executable_in_path(name, &Some(child_path.into()), &mut searched))
}

pub(crate) fn build_child_path() -> String {
    let mut seen = HashSet::new();
    let mut paths = Vec::new();

    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            push_unique_path(&mut paths, &mut seen, dir);
        }
    }

    for dir in [
        "/opt/homebrew/bin",
        "/opt/homebrew/sbin",
        "/usr/local/bin",
        "/usr/local/sbin",
        "/opt/orbstack/bin",
        "/Applications/OrbStack.app/Contents/MacOS/bin",
        "/Applications/Docker.app/Contents/Resources/bin",
        "/opt/local/bin",
        "/opt/local/sbin",
        "/nix/var/nix/profiles/default/bin",
        "/run/current-system/sw/bin",
        "/usr/bin",
        "/bin",
        "/usr/sbin",
        "/sbin",
    ] {
        push_unique_path(&mut paths, &mut seen, PathBuf::from(dir));
    }

    std::env::join_paths(paths)
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}

pub(crate) fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn find_executable_in_path(
    name: &str,
    path: &Option<std::ffi::OsString>,
    searched: &mut Vec<PathBuf>,
) -> Option<PathBuf> {
    let Some(path) = path else {
        return None;
    };

    for dir in std::env::split_paths(path) {
        let candidate = dir.join(name);
        if !searched.iter().any(|path| path == &candidate) {
            searched.push(candidate.clone());
        }
        if is_executable_file(&candidate) {
            return Some(candidate);
        }
    }

    None
}

fn push_unique_path(paths: &mut Vec<PathBuf>, seen: &mut HashSet<PathBuf>, path: PathBuf) {
    if seen.insert(path.clone()) {
        paths.push(path);
    }
}

fn expand_home(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }

    PathBuf::from(path)
}

fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };

    metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_storage_paths_from_current_and_legacy_status() {
        for status in [
            serde_json::json!({"paths": {"appRoot": "/Volumes/Data/container"}}),
            serde_json::json!({"appRoot": "/Volumes/Data/container"}),
        ] {
            assert_eq!(
                apple_data_root_from_status(&status),
                Some(PathBuf::from("/Volumes/Data/container"))
            );
        }
        assert!(
            apple_data_root_from_status(&serde_json::json!({"paths": {"appRoot": "relative"}}))
                .is_none()
        );
        assert!(
            apple_data_root_from_status(&serde_json::json!({"status": "unregistered"})).is_none()
        );
    }

    #[test]
    fn resolves_xkb_data_next_to_an_app_executable() {
        let executable = Path::new("/Applications/Cocoa-Way.app/Contents/MacOS/cocoa-way");
        assert_eq!(
            bundled_xkb_config_root_for(executable),
            Some(PathBuf::from(
                "/Applications/Cocoa-Way.app/Contents/Resources/xkb"
            ))
        );
    }

    #[test]
    fn rejects_incomplete_xkb_data() {
        let directory = tempfile::tempdir().expect("temporary directory");
        std::fs::create_dir_all(directory.path().join("rules")).expect("rules directory");
        std::fs::write(directory.path().join("rules/evdev"), "rules").expect("rules file");
        assert!(!is_xkb_config_root(directory.path()));
    }
}
