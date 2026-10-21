use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

use serde_json::{Map, Value};

pub const GPU_DEFAULT_TTL: u64 = 60;
pub const GPU_MAX_TTL: u64 = 180;
pub const GUI_DEFAULT_TTL: u64 = 600;
pub const GUI_MAX_TTL: u64 = 3600;
pub const GUI_STOP_GRACE_MS: u64 = 2000;
pub const MAX_CHILDREN: usize = 3;
pub const MANIFEST_NAME: &str = "research-manifest.json";
pub const MAX_MANIFEST: usize = 8192;
pub const MAX_HELPER: u64 = 128 * 1024 * 1024;
pub const LIMITS: &str = "Developer-only launch convenience for an author-supplied trusted kit, not a sandbox or secrecy boundary. Helpers must not daemonize or escape their process group. SIGKILL/crash of this launcher cannot guarantee cleanup. TTLs are polling deadlines, not hard real-time guarantees. SHA-256 covers only the helper executable, not the app bundle, dylibs, or runtime behavior. Keep the kit unchanged while running. No permission requests, network setup, downloads, shell, VM boot, or GPU-acceleration API is provided. Native UI reconnect remains incomplete; no compiled GPU backend is claimed.";

#[derive(Debug, Default)]
pub struct Config {
    pub allow_experiments: bool,
    pub root: Option<PathBuf>,
}

impl Config {
    pub fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Self, &'static str> {
        let mut config = Self::default();
        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            match arg.to_str() {
                Some("--allow-experiments") if !config.allow_experiments => {
                    config.allow_experiments = true;
                }
                Some("--root") if config.root.is_none() => {
                    let root = PathBuf::from(args.next().ok_or("missing_root")?);
                    if !valid_root(&root) {
                        return Err("root_must_be_absolute_without_dot_components");
                    }
                    config.root = Some(root);
                }
                _ => {
                    return Err(
                        "usage: cocoa-way-research-mcp [--root ABSOLUTE_KIT] [--allow-experiments]",
                    );
                }
            }
        }
        if config.allow_experiments && config.root.is_none() {
            return Err("allow_experiments_requires_root");
        }
        Ok(config)
    }

    pub fn enabled(&self) -> bool {
        self.allow_experiments && self.root.is_some()
    }
}

pub fn valid_root(path: &Path) -> bool {
    path.is_absolute()
        && path
            .components()
            .any(|part| matches!(part, Component::Normal(_)))
        && path
            .components()
            .all(|part| matches!(part, Component::RootDir | Component::Normal(_)))
        && !path.as_os_str().as_encoded_bytes().contains(&0)
        && !path
            .as_os_str()
            .as_encoded_bytes()
            .split(|byte| *byte == b'/')
            .any(|part| part == b".")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Profile {
    SeamlessHost,
    SeamlessClient,
    GpuSelftest,
}

impl Profile {
    pub const ALL: [Self; MAX_CHILDREN] =
        [Self::SeamlessHost, Self::SeamlessClient, Self::GpuSelftest];

    pub fn parse(id: &str) -> Result<Self, &'static str> {
        Self::ALL
            .into_iter()
            .find(|profile| profile.id() == id)
            .ok_or("unknown_profile")
    }

    pub fn index(self) -> usize {
        self as usize
    }

    pub fn id(self) -> &'static str {
        match self {
            Self::SeamlessHost => "seamless_host",
            Self::SeamlessClient => "seamless_client",
            Self::GpuSelftest => "gpu_selftest",
        }
    }

    pub fn path(self) -> &'static str {
        match self {
            Self::SeamlessHost => "SeamlessHost.app/Contents/MacOS/SeamlessHost",
            Self::SeamlessClient => "SeamlessClient.app/Contents/MacOS/SeamlessClient",
            Self::GpuSelftest => "gpu-vz-probe",
        }
    }

    pub fn argv(self) -> &'static [&'static str] {
        match self {
            Self::SeamlessHost | Self::SeamlessClient => &[],
            Self::GpuSelftest => &["selftest"],
        }
    }

    pub fn is_gui(self) -> bool {
        self != Self::GpuSelftest
    }

    pub fn ttl_limits(self) -> (u64, u64) {
        if self.is_gui() {
            (GUI_DEFAULT_TTL, GUI_MAX_TTL)
        } else {
            (GPU_DEFAULT_TTL, GPU_MAX_TTL)
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum StopAction {
    CleanupGroup,
    RequestAppKit,
    Wait,
}

pub fn stop_action(
    profile: Profile,
    exited: bool,
    grace_expired: bool,
    appkit_result: Option<bool>,
) -> StopAction {
    if exited || !profile.is_gui() || grace_expired {
        StopAction::CleanupGroup
    } else if appkit_result.is_none() {
        StopAction::RequestAppKit
    } else {
        StopAction::Wait
    }
}

#[derive(Clone, Copy)]
pub enum Host {
    MacOs(u32),
    Unsupported,
}

impl Host {
    pub fn check(self) -> Result<(), &'static str> {
        match self {
            Self::MacOs(major) if major >= 27 => Ok(()),
            Self::MacOs(_) => Err("requires_macos_27"),
            Self::Unsupported => Err("unsupported_platform_or_os_version"),
        }
    }

    pub fn detect() -> Self {
        #[cfg(target_os = "macos")]
        {
            let mut bytes = [0u8; 64];
            let mut length = bytes.len();
            // A fixed sysctl read, never a subprocess or permissions probe.
            let result = unsafe {
                libc::sysctlbyname(
                    c"kern.osproductversion".as_ptr(),
                    bytes.as_mut_ptr().cast(),
                    &mut length,
                    std::ptr::null_mut(),
                    0,
                )
            };
            if result == 0 && length <= bytes.len() {
                if let Some(major) = bytes[..length]
                    .split(|byte| *byte == b'.' || *byte == 0)
                    .next()
                    .and_then(|major| std::str::from_utf8(major).ok())
                    .and_then(|major| major.parse().ok())
                {
                    return Self::MacOs(major);
                }
            }
        }
        Self::Unsupported
    }
}

pub fn object_with_keys<'a>(
    value: &'a Value,
    keys: &[&str],
) -> Result<&'a Map<String, Value>, &'static str> {
    let object = value.as_object().ok_or("expected_object")?;
    if object.keys().any(|key| !keys.contains(&key.as_str())) {
        return Err("unexpected_field");
    }
    Ok(object)
}

pub fn launch_arguments(arguments: &Value) -> Result<(Profile, u64), &'static str> {
    let object = object_with_keys(arguments, &["profile", "ttl_seconds"])?;
    let profile = Profile::parse(
        object
            .get("profile")
            .and_then(Value::as_str)
            .ok_or("profile_required")?,
    )?;
    let (default, maximum) = profile.ttl_limits();
    let ttl = match object.get("ttl_seconds") {
        None => default,
        Some(value) => value
            .as_u64()
            .ok_or("ttl_must_be_integer_within_profile_limit")?,
    };
    if !(1..=maximum).contains(&ttl) {
        return Err("ttl_must_be_integer_within_profile_limit");
    }
    Ok((profile, ttl))
}

pub fn stop_arguments(arguments: &Value) -> Result<Profile, &'static str> {
    let object = object_with_keys(arguments, &["profile"])?;
    Profile::parse(
        object
            .get("profile")
            .and_then(Value::as_str)
            .ok_or("profile_required")?,
    )
}

/// Optional to install, mandatory for launch. No manifest-provided paths or argv.
pub struct Manifest(pub [Option<[u8; 32]>; MAX_CHILDREN]);

impl Manifest {
    pub fn parse(bytes: &[u8]) -> Result<Self, &'static str> {
        if bytes.len() > MAX_MANIFEST {
            return Err("manifest_too_large");
        }
        let value: Value = serde_json::from_slice(bytes).map_err(|_| "invalid_manifest_json")?;
        let object = object_with_keys(&value, &["version", "profiles"])?;
        if object.get("version").and_then(Value::as_u64) != Some(1) {
            return Err("unsupported_manifest_version");
        }
        let profiles = object
            .get("profiles")
            .and_then(Value::as_object)
            .ok_or("manifest_profiles_required")?;
        let mut hashes = [None; MAX_CHILDREN];
        for (id, value) in profiles {
            let profile = Profile::parse(id)?;
            let hex = value.as_str().ok_or("invalid_sha256")?.as_bytes();
            if hex.len() != 64
                || !hex
                    .iter()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
            {
                return Err("invalid_sha256");
            }
            let mut hash = [0u8; 32];
            for (index, pair) in hex.chunks_exact(2).enumerate() {
                let digit = |b: u8| {
                    if b.is_ascii_digit() {
                        b - b'0'
                    } else {
                        b - b'a' + 10
                    }
                };
                hash[index] = digit(pair[0]) * 16 + digit(pair[1]);
            }
            hashes[profile.index()] = Some(hash);
        }
        Ok(Self(hashes))
    }
}
