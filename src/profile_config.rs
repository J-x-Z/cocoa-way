//! Lossless, locked, atomic profile configuration transactions.
use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use toml_edit::{ArrayOfTables, DocumentMut, Item, Table, value};

use super::{Config, ContainerSession, ProfileId};

const MAX_CONFIG_BYTES: usize = 1024 * 1024;
const LOCK_WAIT: Duration = Duration::from_millis(100);

fn invalid(error: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}

struct ConfigLock(File);

impl ConfigLock {
    fn acquire(path: &Path) -> io::Result<Self> {
        let parent = path
            .parent()
            .ok_or_else(|| invalid("configuration has no parent directory"))?;
        std::fs::create_dir_all(parent)?;
        let mut name = path
            .file_name()
            .ok_or_else(|| invalid("configuration has no filename"))?
            .to_os_string();
        name.push(".lock");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
            .open(parent.join(name))?;
        if !file.metadata()?.is_file() {
            return Err(invalid("configuration lock is not a regular file"));
        }
        let deadline = Instant::now() + LOCK_WAIT;
        loop {
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                return Ok(Self(file));
            }
            let error = io::Error::last_os_error();
            if !matches!(
                error.kind(),
                io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
            ) {
                return Err(error);
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "configuration is being edited by another process; retry after it finishes",
                ));
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

impl Drop for ConfigLock {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

fn read(path: &Path) -> io::Result<Option<String>> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(invalid("configuration must be a regular file"));
    }
    if metadata.len() > MAX_CONFIG_BYTES as u64 {
        return Err(invalid("configuration exceeds the 1 MiB size limit"));
    }
    let mut content = String::new();
    file.take(MAX_CONFIG_BYTES as u64 + 1)
        .read_to_string(&mut content)?;
    if content.len() > MAX_CONFIG_BYTES {
        return Err(invalid("configuration exceeds the 1 MiB size limit"));
    }
    Ok(Some(content))
}

struct Temporary(PathBuf);
impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn atomic_write(path: &Path, original: Option<&str>, updated: &str) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| invalid("configuration has no parent directory"))?;
    let temporary = Temporary(parent.join(format!(".profiles-{}.tmp", ProfileId::new()?)));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&temporary.0)?;
    if original.is_some() {
        file.set_permissions(std::fs::metadata(path)?.permissions())?;
    }
    file.write_all(updated.as_bytes())?;
    file.sync_all()?;
    // Cooperating writers hold the sidecar flock. Detect non-cooperating edits as well.
    if read(path)?.as_deref() != original {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "configuration changed during transaction; retry without overwriting the newer file",
        ));
    }
    std::fs::rename(&temporary.0, path)?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

fn tables(document: &mut DocumentMut) -> io::Result<&mut ArrayOfTables> {
    if !document.contains_key("session") {
        document["session"] = Item::ArrayOfTables(ArrayOfTables::new());
    }
    if document["session"].is_array() {
        let item = document.remove("session").unwrap();
        let array = item
            .into_array_of_tables()
            .map_err(|_| invalid("session must be an array of tables"))?;
        document["session"] = Item::ArrayOfTables(array);
    }
    document["session"]
        .as_array_of_tables_mut()
        .ok_or_else(|| invalid("session must be an array of tables"))
}

fn decode(document: &DocumentMut) -> io::Result<Vec<ContainerSession>> {
    toml::from_str::<Config>(&document.to_string())
        .map(|config| config.session)
        .map_err(invalid)
}

fn migrate(document: &mut DocumentMut) -> io::Result<()> {
    // Validate every known field and every existing ID before changing anything on disk.
    let sessions = decode(document)?;
    let mut seen = validate_ids(&sessions)?;
    if sessions.iter().all(|session| session.id.is_saved()) {
        return Ok(());
    }
    for table in tables(document)?.iter_mut() {
        if !table.contains_key("id") {
            let id = loop {
                let id = ProfileId::new()?;
                if seen.insert(id) {
                    break id;
                }
            };
            table["id"] = value(id.to_string());
        }
    }
    Ok(())
}

fn validate_ids(sessions: &[ContainerSession]) -> io::Result<HashSet<ProfileId>> {
    let mut seen = HashSet::new();
    for session in sessions {
        if session.id.is_saved() && !seen.insert(session.id) {
            return Err(invalid(format!(
                "duplicate profile ID {}; assign a fresh ID to the copied profile",
                session.id
            )));
        }
    }
    Ok(seen)
}

pub(super) fn transaction<T>(
    path: &Path,
    operation: impl FnOnce(&mut DocumentMut) -> io::Result<T>,
) -> io::Result<T> {
    let _lock = ConfigLock::acquire(path)?;
    let original = read(path)?;
    let initial = original.as_deref().unwrap_or("# cocoa-way container sessions\n# Container Mode owns local Linux GUI sessions.\n# [[session]]\n# name = \"Example\"\n# image = \"example:latest\"\n");
    let mut document = initial.parse::<DocumentMut>().map_err(invalid)?;
    migrate(&mut document)?;
    let result = operation(&mut document)?;
    migrate(&mut document)?;
    let updated = document.to_string();
    if updated.len() > MAX_CONFIG_BYTES {
        return Err(invalid("configuration exceeds the 1 MiB size limit"));
    }
    if original.as_deref() != Some(updated.as_str()) {
        atomic_write(path, original.as_deref(), &updated)?;
    }
    Ok(result)
}

pub(super) fn load(path: &Path) -> io::Result<Vec<ContainerSession>> {
    // Atomic writers publish whole files. Reading a saved configuration needs no
    // sidecar, mkdir, exclusive lock or fsync, including when another writer is busy.
    if let Some(content) = read(path)? {
        let sessions = toml::from_str::<Config>(&content).map_err(invalid)?.session;
        validate_ids(&sessions)?;
        if sessions.iter().all(|session| session.id.is_saved()) {
            return Ok(sessions);
        }
    }
    transaction(path, |document| decode(document))
}

fn position(document: &mut DocumentMut, id: ProfileId) -> io::Result<usize> {
    tables(document)?
        .iter()
        .position(|table| table.get("id").and_then(Item::as_str) == Some(id.to_string().as_str()))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("profile {id} no longer exists"),
            )
        })
}

fn last_position(table: &Table) -> usize {
    let mut last = table.position().unwrap_or_default();
    for (_, item) in table.iter() {
        if let Some(table) = item.as_table() {
            last = last.max(last_position(table));
        } else if let Some(array) = item.as_array_of_tables() {
            for table in array.iter() {
                last = last.max(last_position(table));
            }
        }
    }
    last
}

fn rebase_positions(table: &mut Table, next: &mut usize) {
    table.set_position(*next);
    *next += 1;
    for (_, item) in table.iter_mut() {
        if let Some(table) = item.as_table_mut() {
            rebase_positions(table, next);
        } else if let Some(array) = item.as_array_of_tables_mut() {
            for table in array.iter_mut() {
                rebase_positions(table, next);
            }
        }
    }
}

pub(super) fn append(path: &Path, session: &ContainerSession) -> io::Result<ProfileId> {
    transaction(path, |document| {
        let sessions = decode(document)?;
        super::validate_profile_name(session, &sessions, None)?;
        // Append is creation, including duplication/import: never reuse the source ID.
        let mut created = session.clone();
        created.id = loop {
            let id = ProfileId::new()?;
            if sessions.iter().all(|session| session.id != id) {
                break id;
            }
        };
        let mut serialized = super::session_to_toml(&created)
            .parse::<DocumentMut>()
            .map_err(invalid)?;
        let known = tables(&mut serialized)?.get(0).unwrap().clone();
        let mut table = tables(document)?
            .iter()
            .find(|table| {
                table.get("id").and_then(Item::as_str) == Some(session.id.to_string().as_str())
            })
            .cloned()
            .unwrap_or_else(|| known.clone());
        for key in [
            "id",
            "name",
            "runtime",
            "image",
            "display",
            "presentation",
            "profile",
            "command",
            "app",
            "socket",
            "container_socket",
            "waypipe_path",
            "waypipe_compress",
            "waypipe_threads",
            "audio",
            "runtime_args",
            "mounts",
            "env",
        ] {
            if let Some(item) = known.get(key) {
                table.insert(key, item.clone());
            } else {
                table.remove(key);
            }
        }
        // Parsed/cloned tables retain source positions. Without rebasing, TOML's
        // serializer can put a newly appended profile before existing profiles.
        rebase_positions(&mut table, &mut (last_position(document.as_table()) + 1));
        tables(document)?.push(table);
        Ok(created.id)
    })
}

pub(super) fn replace(path: &Path, id: ProfileId, session: &ContainerSession) -> io::Result<()> {
    transaction(path, |document| {
        let index = position(document, id)?;
        if session.id.is_saved() && session.id != id {
            return Err(invalid(
                "a profile's persistent ID cannot be changed by an edit",
            ));
        }
        super::validate_profile_name(session, &decode(document)?, Some(id))?;
        let mut replacement = session.clone();
        replacement.id = id;
        let mut serialized = super::session_to_toml(&replacement)
            .parse::<DocumentMut>()
            .map_err(invalid)?;
        let replacement = tables(&mut serialized)?.get(0).unwrap();
        let current = tables(document)?.get_mut(index).unwrap();
        for key in [
            "name",
            "runtime",
            "image",
            "display",
            "presentation",
            "profile",
            "command",
            "app",
            "socket",
            "container_socket",
            "waypipe_path",
            "waypipe_compress",
            "waypipe_threads",
            "audio",
            "runtime_args",
            "mounts",
            "env",
        ] {
            match replacement.get(key) {
                Some(item) => {
                    let mut item = item.clone();
                    if let (Some(old), Some(new)) = (
                        current.get(key).and_then(Item::as_value),
                        item.as_value_mut(),
                    ) {
                        *new.decor_mut() = old.decor().clone();
                    }
                    current.insert(key, item);
                }
                None => {
                    current.remove(key);
                }
            }
        }
        Ok(())
    })
}

pub(super) fn remove(path: &Path, id: ProfileId) -> io::Result<()> {
    crate::profile_activity::while_inactive(id, || {
        transaction(path, |document| {
            let index = position(document, id)?;
            tables(document)?.remove(index);
            Ok(())
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::container_sessions::ProfileActionTarget;
    use crate::profile_activity::ProfileActivity;

    fn fixture() -> (tempfile::TempDir, PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("sessions.toml");
        std::fs::write(&path, "# keep this comment\nsetting = 'keep'\n\n[[session]] # inline header\nname = 'Alpha'\nimage = 'test:latest'\ncustom = 'preserved'\ncommand = '''echo hello\n[[session]]\n'''\n[session.extension]\nflag = true\n\n[[session]]\nname = 'Beta'\nimage = 'other:latest'\naudio = false\n\n[settings]\ncolor = 'green' # keep this too\n").unwrap();
        (directory, path)
    }

    #[test]
    fn migration_is_persistent_idempotent_and_lossless_outside_inserted_ids() {
        let (_directory, path) = fixture();
        let original = std::fs::read_to_string(&path).unwrap();
        let sessions = load(&path).unwrap();
        assert_eq!(sessions.len(), 2);
        assert_ne!(sessions[0].id, sessions[1].id);
        let migrated = std::fs::read_to_string(&path).unwrap();
        let without_ids = migrated
            .split_inclusive('\n')
            .filter(|line| !line.starts_with("id = "))
            .collect::<String>();
        assert_eq!(without_ids, original);
        let again = load(&path).unwrap();
        assert_eq!(sessions[0].id, again[0].id);
        assert_eq!(sessions[1].id, again[1].id);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), migrated);
    }

    #[test]
    fn reorder_edit_duplicate_and_delete_use_identity_not_position() {
        let (_directory, path) = fixture();
        let sessions = load(&path).unwrap();
        let alpha = sessions[0].id;
        let beta = sessions[1].id;
        let target = ProfileActionTarget::capture(&sessions[0]);
        transaction(&path, |document| {
            let mut first = tables(document)?.get(0).unwrap().clone();
            tables(document)?.remove(0);
            rebase_positions(&mut first, &mut (last_position(document.as_table()) + 1));
            tables(document)?.push(first);
            Ok(())
        })
        .unwrap();
        assert_eq!(load(&path).unwrap()[0].id, beta);
        assert_eq!(target.resolve(&load(&path).unwrap()).unwrap().id, alpha);
        let mut edited = sessions[0].clone();
        edited.name = "Alpha edited".into();
        replace(&path, alpha, &edited).unwrap();
        assert_eq!(load(&path).unwrap()[1].id, alpha);
        assert!(target.resolve(&load(&path).unwrap()).is_none());
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("custom = 'preserved'"));
        assert!(text.contains("flag = true"));
        assert!(text.contains("color = 'green' # keep this too"));
        edited.name = "Alpha copy".into();
        let copy = append(&path, &edited).unwrap();
        assert_ne!(copy, alpha);
        assert_ne!(copy, beta);
        let document: toml::Value =
            toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(document["session"][2]["custom"].as_str(), Some("preserved"));
        remove(&path, alpha).unwrap();
        let remaining = load(&path).unwrap();
        assert_eq!(
            remaining
                .iter()
                .map(|session| session.id)
                .collect::<Vec<_>>(),
            [beta, copy]
        );
        let before = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            remove(&path, alpha).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        assert_eq!(
            replace(&path, alpha, &edited).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        assert!(target.resolve(&remaining).is_none());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }

    #[test]
    fn invalid_duplicate_ids_or_fields_never_partially_migrate() {
        let (_directory, path) = fixture();
        for invalid in [
            "[[session]]\nname='A'\nimage='a'\nid='11111111111111111111111111111111'\n[[session]]\nname='B'\nimage='b'\nid='11111111111111111111111111111111'\n",
            "[[session]]\nname='A'\nimage='a'\n[[session]]\nname='B'\nimage=123\n",
            "[[session]]\nname='A'\nimage='a'\nid='bad'\n",
            "[[session]]\nname='A'\nimage='a'\nid='00000000000000000000000000000000'\n",
        ] {
            std::fs::write(&path, invalid).unwrap();
            assert!(load(&path).is_err());
            let draft: ContainerSession = toml::from_str("name='Draft'\nimage='d'").unwrap();
            assert!(append(&path, &draft).is_err());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), invalid);
        }
    }

    #[test]
    fn existing_id_cannot_be_edited_and_mixed_legacy_migration_keeps_known_id() {
        let (_directory, path) = fixture();
        let sessions = load(&path).unwrap();
        let mut edited = sessions[0].clone();
        edited.id = sessions[1].id;
        assert!(replace(&path, sessions[0].id, &edited).is_err());
        let original_id = sessions[1].id;
        transaction(&path, |document| {
            tables(document)?.get_mut(0).unwrap().remove("id");
            Ok(())
        })
        .unwrap();
        assert_eq!(load(&path).unwrap()[1].id, original_id);
    }

    #[test]
    fn concurrent_unrelated_deletion_does_not_retarget_running_or_pending_profiles() {
        let (_directory, path) = fixture();
        let sessions = load(&path).unwrap();
        let alpha = sessions[0].id;
        let beta = sessions[1].id;
        let running = ProfileActivity::begin(beta);
        let pending = ProfileActivity::begin(beta);
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let worker_barrier = barrier.clone();
        let worker_path = path.clone();
        let worker = std::thread::spawn(move || {
            worker_barrier.wait();
            remove(&worker_path, alpha)
        });
        barrier.wait();
        worker.join().unwrap().unwrap();
        assert_eq!(load(&path).unwrap()[0].id, beta);
        assert!(remove(&path, beta).is_err());
        drop(running);
        assert!(remove(&path, beta).is_err());
        drop(pending);
        remove(&path, beta).unwrap();
        assert!(load(&path).unwrap().is_empty());
    }

    #[test]
    fn cooperating_concurrent_writers_do_not_lose_changes_or_ids() {
        let (_directory, path) = fixture();
        let original = load(&path).unwrap();
        let threads = (0..8)
            .map(|index| {
                let path = path.clone();
                std::thread::spawn(move || {
                    let draft = toml::from_str::<ContainerSession>(&format!(
                        "name='Concurrent {index}'\nimage='test'\n"
                    ))
                    .unwrap();
                    let deadline = Instant::now() + Duration::from_secs(3);
                    loop {
                        match append(&path, &draft) {
                            Ok(id) => break id,
                            Err(error)
                                if error.kind() == io::ErrorKind::WouldBlock
                                    && Instant::now() < deadline =>
                            {
                                std::thread::sleep(Duration::from_millis(1));
                            }
                            Err(error) => panic!("concurrent append failed: {error}"),
                        }
                    }
                })
            })
            .collect::<Vec<_>>();
        let ids = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect::<HashSet<_>>();
        let sessions = load(&path).unwrap();
        assert_eq!(ids.len(), 8);
        assert_eq!(sessions.len(), 10);
        assert_eq!(sessions[0].id, original[0].id);
        assert_eq!(sessions[1].id, original[1].id);
        assert!(
            ids.iter()
                .all(|id| sessions.iter().any(|session| session.id == *id))
        );
    }

    #[test]
    fn noncooperating_edits_and_symlinks_are_not_overwritten() {
        let (directory, path) = fixture();
        load(&path).unwrap();
        assert!(
            transaction(&path, |document| {
                document["setting"] = value("ours");
                std::fs::write(&path, "# concurrent editor\nsetting = 'theirs'\n")?;
                Ok(())
            })
            .is_err()
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "# concurrent editor\nsetting = 'theirs'\n"
        );
        let link = directory.path().join("linked.toml");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(load(&link).is_err());
        assert!(!std::fs::read_dir(directory.path()).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp")
        }));
    }

    #[test]
    fn inline_session_arrays_migrate_without_losing_other_tables() {
        let (_directory, path) = fixture();
        std::fs::write(
            &path,
            "session = [{name='Inline', image='test', extra='keep'}]\n[settings]\nvalue=42\n",
        )
        .unwrap();
        let sessions = load(&path).unwrap();
        assert!(sessions[0].id.is_saved());
        let value: toml::Value = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(value["settings"]["value"].as_integer(), Some(42));
        assert_eq!(value["session"][0]["extra"].as_str(), Some("keep"));
    }

    #[test]
    fn saved_load_is_lock_free_and_does_not_touch_file_or_sidecar() {
        let (_directory, path) = fixture();
        let sessions = load(&path).unwrap();
        let before = std::fs::metadata(&path).unwrap().modified().unwrap();
        let _held = ConfigLock::acquire(&path).unwrap();
        let started = Instant::now();
        let loaded = load(&path).unwrap();
        assert!(started.elapsed() < LOCK_WAIT);
        assert_eq!(loaded[0].id, sessions[0].id);
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            before
        );
        // A broken lock path is irrelevant to read-only loads as well.
        std::fs::remove_file(path.with_extension("toml.lock")).unwrap();
        std::os::unix::fs::symlink(&path, path.with_extension("toml.lock")).unwrap();
        assert_eq!(load(&path).unwrap()[0].id, sessions[0].id);
    }

    #[test]
    fn held_lock_bounds_migration_and_mutation_without_partial_writes() {
        let (_directory, path) = fixture();
        let original = std::fs::read_to_string(&path).unwrap();
        let held = ConfigLock::acquire(&path).unwrap();
        let started = Instant::now();
        assert_eq!(load(&path).unwrap_err().kind(), io::ErrorKind::WouldBlock);
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        drop(held);
        let sessions = load(&path).unwrap();
        let original = std::fs::read_to_string(&path).unwrap();
        let _held = ConfigLock::acquire(&path).unwrap();
        let started = Instant::now();
        assert_eq!(
            replace(&path, sessions[0].id, &sessions[0])
                .unwrap_err()
                .kind(),
            io::ErrorKind::WouldBlock
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }

    #[test]
    fn config_reads_reject_oversize_and_special_files_without_blocking() {
        let (directory, path) = fixture();
        std::fs::write(&path, vec![b' '; MAX_CONFIG_BYTES + 1]).unwrap();
        assert_eq!(load(&path).unwrap_err().kind(), io::ErrorKind::InvalidData);
        let fifo = directory.path().join("fifo.toml");
        let name = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let started = Instant::now();
        assert_eq!(load(&fifo).unwrap_err().kind(), io::ErrorKind::InvalidData);
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
