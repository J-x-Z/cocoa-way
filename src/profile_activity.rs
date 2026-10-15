use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use crate::container_sessions::ProfileId;

static OPERATIONS: LazyLock<Mutex<HashMap<ProfileId, usize>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static IDENTITIES: LazyLock<Mutex<HashMap<(String, String), (ProfileId, u64)>>> =
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
    owner: (ProfileId, u64),
}

pub fn reserve_identity(
    id: ProfileId,
    generation: u64,
    runtime: &str,
    name: &str,
) -> Result<RuntimeReservation, String> {
    if !id.is_saved() {
        return Err("an unsaved profile cannot reserve a runtime identity".into());
    }
    let key = (runtime_key(runtime).into(), name.into());
    let mut identities = IDENTITIES.lock().unwrap_or_else(|error| error.into_inner());
    if identities.contains_key(&key) {
        return Err(format!(
            "Container identity '{name}' is reserved by another active or pending instance. Wait for cleanup or choose a distinct name."
        ));
    }
    identities.insert(key.clone(), (id, generation));
    Ok(RuntimeReservation {
        key,
        owner: (id, generation),
    })
}

pub fn identity_reserved_by_other(runtime: &str, name: &str, replacing: Option<ProfileId>) -> bool {
    IDENTITIES
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .get(&(runtime_key(runtime).into(), name.into()))
        .is_some_and(|(id, _)| replacing != Some(*id))
}

impl Drop for RuntimeReservation {
    fn drop(&mut self) {
        let mut identities = IDENTITIES.lock().unwrap_or_else(|error| error.into_inner());
        if identities.get(&self.key) == Some(&self.owner) {
            identities.remove(&self.key);
        }
    }
}

/// A profile-specific lease retained by checks, launches, instances and asynchronous cleanup.
/// Acquire before resolving the current profile; a deleted ID must never launch a snapshot.
#[derive(Debug)]
pub struct ProfileActivity {
    id: ProfileId,
}

impl ProfileActivity {
    pub fn begin(id: ProfileId) -> Self {
        assert!(id.is_saved(), "activity requires a persistent profile ID");
        *OPERATIONS
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .entry(id)
            .or_default() += 1;
        Self { id }
    }

    pub fn profile_id(&self) -> ProfileId {
        self.id
    }
}

impl Drop for ProfileActivity {
    fn drop(&mut self) {
        let mut operations = OPERATIONS.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(count) = operations.get_mut(&self.id) {
            *count -= 1;
            if *count == 0 {
                operations.remove(&self.id);
            }
        }
    }
}

pub fn while_inactive<T>(
    id: ProfileId,
    operation: impl FnOnce() -> std::io::Result<T>,
) -> std::io::Result<T> {
    let operations = OPERATIONS.lock().unwrap_or_else(|error| error.into_inner());
    if operations.get(&id).copied().unwrap_or_default() != 0 {
        return Err(std::io::Error::other(
            "Stop this application and wait for its pending checks, launches and cleanup before deleting its profile.",
        ));
    }
    // Held through the atomic configuration replacement, not just the in-memory edit.
    operation()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activity_only_blocks_its_own_profile_and_last_lease_releases_it() {
        let first = ProfileId::new().unwrap();
        let second = ProfileId::new().unwrap();
        let launch = ProfileActivity::begin(first);
        let cleanup = ProfileActivity::begin(first);
        assert!(while_inactive(second, || Ok(())).is_ok());
        assert!(while_inactive(first, || Ok(())).is_err());
        drop(launch);
        assert!(while_inactive(first, || Ok(())).is_err());
        drop(cleanup);
        assert!(while_inactive(first, || Ok(())).is_ok());
    }

    #[test]
    fn original_name_stays_reserved_through_profile_rename_and_cleanup() {
        let name = "audit-reservation-rename";
        let first = ProfileId::new().unwrap();
        let second = ProfileId::new().unwrap();
        let original = reserve_identity(first, 1, "apple", name).unwrap();
        assert!(identity_reserved_by_other("container", name, None));
        assert!(!identity_reserved_by_other("container", name, Some(first)));
        assert!(reserve_identity(second, 2, "container", name).is_err());
        drop(original);
        assert!(!identity_reserved_by_other("container", name, None));
        assert!(reserve_identity(second, 2, "container", name).is_ok());
    }
}
