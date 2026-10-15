use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

static OPERATIONS: Mutex<usize> = Mutex::new(0);
static IDENTITIES: LazyLock<Mutex<HashMap<(String, String), (usize, u64)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn runtime_key(runtime: &str) -> &str {
    match runtime.trim() {
        "apple" | "apple-container" => "container",
        "orbstack" => "orb",
        runtime => runtime,
    }
}

#[derive(Debug)]
pub struct RuntimeReservation {
    key: (String, String),
    owner: (usize, u64),
}

pub fn reserve_identity(
    index: usize,
    generation: u64,
    runtime: &str,
    name: &str,
) -> Result<RuntimeReservation, String> {
    let key = (runtime_key(runtime).into(), name.into());
    let mut identities = IDENTITIES.lock().unwrap_or_else(|error| error.into_inner());
    if identities.contains_key(&key) {
        return Err(format!(
            "Container identity '{name}' is reserved by another active or pending instance. Wait for cleanup or choose a distinct name."
        ));
    }
    identities.insert(key.clone(), (index, generation));
    Ok(RuntimeReservation {
        key,
        owner: (index, generation),
    })
}

pub fn identity_reserved_by_other(runtime: &str, name: &str, replacing: Option<usize>) -> bool {
    IDENTITIES
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .get(&(runtime_key(runtime).into(), name.into()))
        .is_some_and(|(index, _)| replacing != Some(*index))
}

impl Drop for RuntimeReservation {
    fn drop(&mut self) {
        let mut identities = IDENTITIES.lock().unwrap_or_else(|error| error.into_inner());
        if identities.get(&self.key) == Some(&self.owner) {
            identities.remove(&self.key);
        }
    }
}

/// Keeps positional configuration entries fixed while a worker or instance owns them.
#[derive(Debug)]
pub struct ProfileActivity {
    _private: (),
}

impl ProfileActivity {
    pub fn begin() -> Self {
        *OPERATIONS.lock().unwrap_or_else(|error| error.into_inner()) += 1;
        Self { _private: () }
    }
}

impl Drop for ProfileActivity {
    fn drop(&mut self) {
        let mut count = OPERATIONS.lock().unwrap_or_else(|error| error.into_inner());
        *count -= 1;
    }
}

pub fn while_idle<T>(operation: impl FnOnce() -> std::io::Result<T>) -> std::io::Result<T> {
    let count = OPERATIONS.lock().unwrap_or_else(|error| error.into_inner());
    require_idle(*count)?;
    // Hold the lock through the write: no worker can reserve an index mid-deletion.
    operation()
}

fn require_idle(count: usize) -> std::io::Result<()> {
    if count == 0 {
        Ok(())
    } else {
        Err(std::io::Error::other(
            "Stop all application instances and wait for pending checks/launches before deleting a profile.",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_compaction_requires_no_workers_or_instances() {
        assert!(require_idle(0).is_ok());
        assert!(require_idle(1).is_err());
        assert!(require_idle(2).is_err());
    }

    #[test]
    fn original_name_stays_reserved_through_profile_rename_and_cleanup() {
        let name = "audit-reservation-rename";
        let original = reserve_identity(42, 1, "apple", name).unwrap();
        assert!(identity_reserved_by_other("container", name, None));
        assert!(!identity_reserved_by_other("container", name, Some(42)));
        assert!(reserve_identity(43, 2, "container", name).is_err());
        drop(original);
        assert!(!identity_reserved_by_other("container", name, None));
        assert!(reserve_identity(43, 2, "container", name).is_ok());
    }
}
