//! Descriptor-relative, no-follow inspection. Same-UID mutation and mutable bundle
//! resources remain outside this trusted-kit launcher's protection boundary.
use std::ffi::CString;
use std::fs::{File, Metadata};
use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

use sha2::{Digest, Sha256};

use super::policy::{MANIFEST_NAME, MAX_HELPER, MAX_MANIFEST, Manifest, Profile, valid_root};

pub struct Kit {
    root: File,
    path: PathBuf,
    manifest_file: File,
    manifest_stamp: Metadata,
    manifest: Manifest,
}

pub struct Helper {
    pub path: PathBuf,
    file: File,
    stamp: Metadata,
}

fn open_at(
    directory: &File,
    name: &std::ffi::OsStr,
    is_directory: bool,
) -> Result<File, &'static str> {
    let name = CString::new(name.as_bytes()).map_err(|_| "invalid_kit_path")?;
    let flags = libc::O_RDONLY
        | libc::O_NOFOLLOW
        | libc::O_CLOEXEC
        | libc::O_NONBLOCK
        | if is_directory { libc::O_DIRECTORY } else { 0 };
    // File owns the new descriptor; O_NONBLOCK also prevents FIFO-open hangs.
    let fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) };
    if fd < 0 {
        return Err("missing_or_unsafe_kit_asset");
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn owned(file: &File, directory: bool, executable: bool) -> Result<Metadata, &'static str> {
    let metadata = file.metadata().map_err(|_| "asset_metadata_unavailable")?;
    if metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o6022 != 0
        || if directory {
            !metadata.is_dir()
        } else {
            !metadata.is_file()
        }
        || (executable && metadata.mode() & 0o100 == 0)
    {
        return Err("asset_requires_current_uid_no_group_other_write_and_safe_type");
    }
    Ok(metadata)
}

fn unchanged(before: &Metadata, after: &Metadata) -> bool {
    before.dev() == after.dev()
        && before.ino() == after.ino()
        && before.uid() == after.uid()
        && before.mode() == after.mode()
        && before.len() == after.len()
        && before.mtime() == after.mtime()
        && before.mtime_nsec() == after.mtime_nsec()
        && before.ctime() == after.ctime()
        && before.ctime_nsec() == after.ctime_nsec()
}

fn root(path: &Path) -> Result<File, &'static str> {
    if !valid_root(path)
        || unsafe { libc::getuid() != libc::geteuid() || libc::getgid() != libc::getegid() }
    {
        return Err("unsafe_root_or_elevated_launcher");
    }
    let mut directory = File::open("/").map_err(|_| "root_unavailable")?;
    for part in path.components() {
        if let Component::Normal(name) = part {
            directory = open_at(&directory, name, true)?;
        }
    }
    owned(&directory, true, false)?;
    Ok(directory)
}

impl Kit {
    pub fn open(path: &Path) -> Result<Self, &'static str> {
        let root = root(path)?;
        let mut manifest_file = open_at(&root, MANIFEST_NAME.as_ref(), false)?;
        let manifest_stamp = owned(&manifest_file, false, false)?;
        if manifest_stamp.len() > MAX_MANIFEST as u64 {
            return Err("manifest_too_large");
        }
        let mut bytes = Vec::new();
        (&mut manifest_file)
            .take(MAX_MANIFEST as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| "manifest_read_failed")?;
        let manifest = Manifest::parse(&bytes)?;
        if !unchanged(&manifest_stamp, &owned(&manifest_file, false, false)?) {
            return Err("manifest_changed");
        }
        Ok(Self {
            root,
            path: path.to_path_buf(),
            manifest_file,
            manifest_stamp,
            manifest,
        })
    }

    fn helper(&self, profile: Profile) -> Result<Helper, &'static str> {
        if self.manifest.0[profile.index()].is_none() {
            return Err("profile_hash_missing");
        }
        let parts: Vec<_> = Path::new(profile.path()).components().collect();
        let mut directory = self.root.try_clone().map_err(|_| "root_unavailable")?;
        for (index, part) in parts.iter().enumerate() {
            let Component::Normal(name) = part else {
                return Err("invalid_fixed_profile_path");
            };
            let is_directory = index + 1 != parts.len();
            let file = open_at(&directory, name, is_directory)?;
            let stamp = owned(&file, is_directory, !is_directory)?;
            if !is_directory {
                if stamp.len() == 0 || stamp.len() > MAX_HELPER {
                    return Err("helper_size_out_of_bounds");
                }
                return Ok(Helper {
                    path: self.path.join(profile.path()),
                    file,
                    stamp,
                });
            }
            directory = file;
        }
        Err("invalid_fixed_profile_path")
    }

    pub fn inspect(&self, profile: Profile) -> Result<(), &'static str> {
        self.helper(profile).map(|_| ())
    }

    pub fn verify(
        &self,
        profile: Profile,
        mut upkeep: impl FnMut(),
    ) -> Result<Helper, &'static str> {
        let mut helper = self.helper(profile)?;
        let mut digest = Sha256::new();
        let mut bytes = [0u8; 64 * 1024];
        let mut total = 0u64;
        loop {
            upkeep();
            if super::transport::shutdown_requested() {
                return Err("server_shutting_down");
            }
            let count = helper
                .file
                .read(&mut bytes)
                .map_err(|_| "helper_read_failed")?;
            if count == 0 {
                break;
            }
            total += count as u64;
            if total > MAX_HELPER {
                return Err("helper_size_out_of_bounds");
            }
            digest.update(&bytes[..count]);
        }
        let actual: [u8; 32] = digest.finalize().into();
        if Some(actual) != self.manifest.0[profile.index()] {
            return Err("sha256_mismatch");
        }
        self.recheck(profile, &helper)?;
        Ok(helper)
    }

    pub fn recheck(&self, profile: Profile, helper: &Helper) -> Result<(), &'static str> {
        let reopened = root(&self.path)?;
        let original = self.root.metadata().map_err(|_| "root_unavailable")?;
        let current = reopened.metadata().map_err(|_| "root_unavailable")?;
        if original.dev() != current.dev() || original.ino() != current.ino() {
            return Err("root_changed");
        }
        let manifest = open_at(&reopened, MANIFEST_NAME.as_ref(), false)?;
        if !unchanged(&self.manifest_stamp, &owned(&manifest, false, false)?)
            || !unchanged(
                &self.manifest_stamp,
                &owned(&self.manifest_file, false, false)?,
            )
            || !unchanged(&helper.stamp, &owned(&helper.file, false, true)?)
            || !unchanged(&helper.stamp, &self.helper(profile)?.stamp)
        {
            return Err("kit_changed_during_verification");
        }
        Ok(())
    }

    #[cfg(target_os = "macos")]
    pub fn root_fd(&self) -> libc::c_int {
        self.root.as_raw_fd()
    }
}
