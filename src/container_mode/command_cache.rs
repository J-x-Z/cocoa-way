use std::collections::{HashMap, HashSet};

pub(super) struct CommandCache<T> {
    entries: HashMap<String, T>,
    refreshing: HashSet<(u64, String)>,
    generation: u64,
}

impl<T> CommandCache<T> {
    pub(super) fn new() -> Self {
        Self {
            entries: HashMap::new(),
            refreshing: HashSet::new(),
            generation: 0,
        }
    }

    pub(super) fn get(&self, key: &str) -> Option<&T> {
        self.entries.get(key)
    }

    pub(super) fn begin_refresh(&mut self, key: &str) -> Option<u64> {
        self.refreshing
            .insert((self.generation, key.to_owned()))
            .then_some(self.generation)
    }

    pub(super) fn finish_refresh(&mut self, key: String, generation: u64, entry: T) -> bool {
        let pending = self.refreshing.remove(&(generation, key.clone()));
        if !pending || generation != self.generation {
            return false;
        }
        self.entries.insert(key, entry);
        true
    }

    pub(super) fn invalidate(&mut self) {
        self.entries.clear();
        // In-flight probes may describe the runtime before a completed action.
        self.generation = self.generation.wrapping_add(1);
        self.refreshing.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_cache_deduplicates_refreshes_and_keeps_last_result() {
        let mut cache = CommandCache::new();
        let generation = cache.begin_refresh("images").unwrap();
        assert_eq!(cache.begin_refresh("images"), None);
        assert!(cache.finish_refresh("images".into(), generation, "old images"));
        let generation = cache.begin_refresh("images").unwrap();
        assert_eq!(cache.get("images"), Some(&"old images"));
        assert!(cache.finish_refresh("images".into(), generation, "new images"));
        assert_eq!(cache.get("images"), Some(&"new images"));
    }

    #[test]
    fn command_cache_invalidated_probe_cannot_repopulate_or_cancel_new_probe() {
        let mut cache = CommandCache::new();
        let old = cache.begin_refresh("containers").unwrap();
        cache.invalidate();
        let current = cache.begin_refresh("containers").unwrap();
        assert_ne!(old, current);
        assert!(!cache.finish_refresh("containers".into(), old, "before stop"));
        assert_eq!(cache.get("containers"), None);
        assert_eq!(cache.begin_refresh("containers"), None);
        assert!(cache.finish_refresh("containers".into(), current, "after stop"));
        assert_eq!(cache.get("containers"), Some(&"after stop"));
    }

    #[test]
    fn command_cache_late_old_probe_cannot_overwrite_completed_new_probe() {
        let mut cache = CommandCache::new();
        let old = cache.begin_refresh("volumes").unwrap();
        cache.invalidate();
        let current = cache.begin_refresh("volumes").unwrap();
        assert!(cache.finish_refresh("volumes".into(), current, "after delete"));
        assert!(!cache.finish_refresh("volumes".into(), old, "before delete"));
        assert_eq!(cache.get("volumes"), Some(&"after delete"));
        cache.invalidate();
        assert_eq!(cache.get("volumes"), None);
    }
}
