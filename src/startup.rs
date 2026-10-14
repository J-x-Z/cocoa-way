use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::mpsc::SyncSender;

use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::prelude::*;

const LOG_LIMIT: u64 = 256 * 1024;
const RECORD_LIMIT: usize = 8 * 1024;

pub fn log_directory() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(|home| PathBuf::from(home).join("Library/Logs/Cocoa-Way"))
}

struct RotatingLog {
    path: PathBuf,
    file: std::fs::File,
    size: u64,
    limit: u64,
}

impl RotatingLog {
    fn open(path: PathBuf, limit: u64) -> io::Result<Self> {
        let file = Self::open_file(&path)?;
        let size = file.metadata()?.len();
        Ok(Self {
            path,
            file,
            size,
            limit,
        })
    }

    fn open_file(path: &Path) -> io::Result<std::fs::File> {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
    }

    fn write_record(&mut self, record: &[u8]) -> io::Result<()> {
        let record = &record[..record.len().min(RECORD_LIMIT)];
        if self.size.saturating_add(record.len() as u64) > self.limit {
            let previous = self.path.with_extension("previous.log");
            std::fs::rename(&self.path, &previous)?;
            self.file = Self::open_file(&self.path)?;
            self.size = 0;
        }
        self.file.write_all(record)?;
        self.size += record.len() as u64;
        Ok(())
    }
}

#[derive(Clone)]
struct DiagnosticWriter {
    sender: SyncSender<Vec<u8>>,
}

struct DiagnosticRecord {
    sender: SyncSender<Vec<u8>>,
    bytes: Vec<u8>,
}

impl Write for DiagnosticRecord {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let remaining = RECORD_LIMIT.saturating_sub(self.bytes.len());
        self.bytes
            .extend_from_slice(&buffer[..buffer.len().min(remaining)]);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for DiagnosticRecord {
    fn drop(&mut self) {
        if !self.bytes.is_empty() {
            // Diagnostics must not block input or grow an unbounded queue.
            let _ = self.sender.try_send(std::mem::take(&mut self.bytes));
        }
    }
}

impl<'a> MakeWriter<'a> for DiagnosticWriter {
    type Writer = DiagnosticRecord;

    fn make_writer(&'a self) -> Self::Writer {
        DiagnosticRecord {
            sender: self.sender.clone(),
            bytes: Vec::new(),
        }
    }
}

fn diagnostic_writer() -> io::Result<DiagnosticWriter> {
    let directory = log_directory().ok_or_else(|| io::Error::other("HOME is not set"))?;
    prepare_private_directory(&directory)?;
    let mut log = RotatingLog::open(directory.join("cocoa-way.log"), LOG_LIMIT)?;
    log.write_record(
        format!(
            "\nCocoa-Way {} startup; host={}/{}; pid={}\n",
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS,
            std::env::consts::ARCH,
            std::process::id()
        )
        .as_bytes(),
    )?;
    let (sender, receiver) = std::sync::mpsc::sync_channel::<Vec<u8>>(128);
    std::thread::Builder::new()
        .name("cocoa-way-diagnostics-log".into())
        .spawn(move || {
            while let Ok(record) = receiver.recv() {
                if let Err(error) = log.write_record(&record) {
                    eprintln!("Cocoa-Way diagnostic log is unavailable: {error}");
                    break;
                }
            }
        })?;
    Ok(DiagnosticWriter { sender })
}

pub fn initialize_logging(filter: &str, worker: bool) {
    let filter = tracing_subscriber::EnvFilter::try_new(filter).unwrap_or_else(|error| {
        eprintln!("Ignoring invalid RUST_LOG: {error}");
        tracing_subscriber::EnvFilter::new("cocoa_way=info,smithay=warn")
    });
    let console = tracing_subscriber::fmt::layer()
        .with_writer(io::stderr)
        .with_filter(filter);
    let file = if worker {
        None
    } else {
        match diagnostic_writer() {
            Ok(writer) => Some(
                tracing_subscriber::fmt::layer()
                    .with_ansi(false)
                    .with_writer(writer)
                    .with_filter(tracing_subscriber::filter::LevelFilter::WARN),
            ),
            Err(error) => {
                eprintln!("Cocoa-Way could not create its diagnostic log: {error}");
                None
            }
        }
    };
    if let Err(error) = tracing_subscriber::registry()
        .with(console)
        .with(file)
        .try_init()
    {
        eprintln!("Cocoa-Way could not initialize logging: {error}");
    }
}

pub fn prepare_private_directory(path: &Path) -> io::Result<()> {
    if !path.is_absolute() {
        return Err(io::Error::other("directory path must be absolute"));
    }
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)?;
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.uid() != unsafe { libc::geteuid() } {
        return Err(io::Error::other(
            "directory must be owned by the current user and must not be a symlink",
        ));
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
}

pub fn show_display_at_startup(worker: bool, rootless: bool) -> bool {
    worker && !rootless
}

pub fn validate_home(home: Option<&std::ffi::OsStr>) -> Result<(), String> {
    let home = home.filter(|home| !home.is_empty()).ok_or_else(||
        "HOME is unavailable. Launch Cocoa-Way from your macOS user account so profiles and diagnostic logs have a valid home directory.".to_string()
    )?;
    if !Path::new(home).is_absolute() || !Path::new(home).is_dir() {
        return Err("HOME must name an existing absolute directory. Correct the launcher environment before starting Cocoa-Way.".into());
    }
    Ok(())
}

pub fn environment_report() -> String {
    let child_path = crate::runtime_paths::build_child_path();
    let mut lines = vec![
        format!(
            "Cocoa-Way {} ({}/{})",
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS,
            std::env::consts::ARCH
        ),
        "Metal and keyboard data are validated when the compositor starts.".into(),
        "Runtime tools are optional until you choose their connection type.".into(),
        String::new(),
    ];
    for (name, purpose, hint) in [
        (
            "waypipe",
            "transported applications",
            "brew install J-x-Z/tap/waypipe-darwin",
        ),
        (
            "ssh",
            "remote Linux connections",
            "enable or restore the macOS OpenSSH client",
        ),
        (
            "container",
            "Apple Container applications",
            "install Apple's Container runtime separately",
        ),
        (
            "docker",
            "Docker-compatible providers",
            "install the provider's Docker CLI",
        ),
        (
            "orbctl",
            "OrbStack machines",
            "install OrbStack if you use this provider",
        ),
    ] {
        if let Some(path) = crate::runtime_paths::find_command_path(name, &child_path) {
            lines.push(format!("{name}: {}", path.display()));
        } else {
            lines.push(format!("{name}: not found ({purpose}); {hint}."));
        }
    }
    if let Some(root) = std::env::var_os("XKB_CONFIG_ROOT") {
        lines.push(format!("Keyboard data: {}", PathBuf::from(root).display()));
    }
    if let Some(directory) = log_directory() {
        lines.push(format!(
            "Warnings and errors: {}",
            directory.join("cocoa-way.log").display()
        ));
    }
    lines.push("This check does not start runtimes or containers. Use the selected application's Check action to validate its image and runtime service.".into());
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_startup_has_no_empty_display_but_desktop_workers_do() {
        assert!(!show_display_at_startup(false, false));
        assert!(!show_display_at_startup(false, true));
        assert!(show_display_at_startup(true, false));
        assert!(!show_display_at_startup(true, true));
    }

    #[test]
    fn missing_or_relative_home_is_an_actionable_startup_error() {
        assert!(
            validate_home(None)
                .unwrap_err()
                .contains("macOS user account")
        );
        assert!(validate_home(Some(std::ffi::OsStr::new(""))).is_err());
        assert!(validate_home(Some(std::ffi::OsStr::new("relative"))).is_err());
        let directory = tempfile::tempdir().unwrap();
        assert!(validate_home(Some(directory.path().as_os_str())).is_ok());
    }

    #[test]
    fn diagnostics_are_bounded_and_rotate_once() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("cocoa-way.log");
        let mut log = RotatingLog::open(path.clone(), 8).unwrap();
        log.write_record(b"first\n").unwrap();
        log.write_record(b"second\n").unwrap();
        log.write_record(b"third\n").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"third\n");
        assert_eq!(
            std::fs::read(path.with_extension("previous.log")).unwrap(),
            b"second\n"
        );
    }

    #[test]
    fn diagnostic_writer_does_not_wait_for_a_full_queue() {
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        sender.try_send(b"first".to_vec()).unwrap();
        let writer = DiagnosticWriter { sender };
        let mut record = writer.make_writer();
        record.write_all(b"second").unwrap();
        drop(record);
        assert_eq!(receiver.try_recv().unwrap(), b"first");
        assert!(receiver.try_recv().is_err());
        assert!(writer.sender.try_send(vec![]).is_ok());
    }

    #[test]
    fn private_runtime_directory_is_not_a_symlink_or_regular_file() {
        let directory = tempfile::tempdir().unwrap();
        let private = directory.path().join("private");
        prepare_private_directory(&private).unwrap();
        assert_eq!(
            std::fs::metadata(&private).unwrap().permissions().mode() & 0o777,
            0o700
        );
        let link = directory.path().join("link");
        std::os::unix::fs::symlink(&private, &link).unwrap();
        assert!(prepare_private_directory(&link).is_err());
        let file = directory.path().join("file");
        std::fs::write(&file, b"occupied").unwrap();
        assert!(prepare_private_directory(&file).is_err());
        assert!(prepare_private_directory(Path::new("relative")).is_err());
    }
}
