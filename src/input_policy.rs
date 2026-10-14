//! Pure input policy; callers own key mapping, forwarding, focus, and native grabs.
//!
//! Key codes are evdev codes without the XKB +8 offset. Release drained keys to
//! the previous focus before applying an owner change to the actual keyboard.

use std::collections::BTreeSet;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyTransition {
    Pressed,
    Released,
}

/// Tracks accepted physical key transitions for one native window at a time.
#[derive(Debug)]
pub struct KeyTransitionReducer<Owner = u64> {
    owner: Option<Owner>,
    held_keys: BTreeSet<u32>,
}

impl<Owner> Default for KeyTransitionReducer<Owner> {
    fn default() -> Self {
        Self {
            owner: None,
            held_keys: BTreeSet::new(),
        }
    }
}

impl<Owner: Copy + Eq> KeyTransitionReducer<Owner> {
    pub fn owner(&self) -> Option<Owner> {
        self.owner
    }

    /// Changes ownership and returns the previous owner's held evdev keys.
    /// Keys are sorted, drained exactly once, and never transferred to the new
    /// owner. Setting the same owner preserves held keys and returns nothing.
    #[must_use = "release the drained keys to the previous keyboard focus"]
    pub fn set_owner(&mut self, owner: Option<Owner>) -> Vec<u32> {
        if self.owner == owner {
            return Vec::new();
        }
        self.owner = owner;
        std::mem::take(&mut self.held_keys).into_iter().collect()
    }

    /// Clears ownership and drains held keys only if this window still owns
    /// the keyboard. A stale loss event cannot release another owner's keys.
    #[must_use = "release the drained keys before clearing keyboard focus"]
    pub fn focus_lost(&mut self, owner: Owner) -> Vec<u32> {
        if self.owner != Some(owner) {
            return Vec::new();
        }
        self.set_owner(None)
    }

    /// No target means no new presses, but accepted releases must still clear XKB.
    pub fn key_with_focus(
        &mut self,
        owner: Owner,
        evdev_key: u32,
        transition: KeyTransition,
        repeat: bool,
        has_focus: bool,
    ) -> bool {
        if !has_focus && transition == KeyTransition::Pressed {
            return false;
        }
        self.key(owner, evdev_key, transition, repeat)
    }

    /// Returns true only when the caller should forward this transition.
    /// Rejects other owners, repeated/duplicate presses, and unmatched releases.
    /// A repeat flag never prevents releasing an accepted press.
    #[must_use = "forward only accepted key transitions"]
    pub fn key(
        &mut self,
        owner: Owner,
        evdev_key: u32,
        transition: KeyTransition,
        repeat: bool,
    ) -> bool {
        if self.owner != Some(owner) {
            return false;
        }
        match transition {
            KeyTransition::Pressed => !repeat && self.held_keys.insert(evdev_key),
            KeyTransition::Released => self.held_keys.remove(&evdev_key),
        }
    }
}

/// Consume a suspended selection without overriding a new click or reviving a dead target.
pub fn resume_focus<Focus>(
    saved: &mut Option<Focus>,
    already_focused: bool,
    live: impl FnOnce(&Focus) -> bool,
) -> Option<Focus> {
    let selected = saved.take();
    if already_focused {
        return None;
    }
    selected.filter(live)
}

/// Caller-supplied prerequisites for activating a pointer lock, not a grab
/// implementation or evidence of raw-input or confinement support.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PointerLockEligibility {
    /// The requested surface has pointer focus and its native window is focused.
    pub focused: bool,
    /// The pointer is inside the effective, committed surface-local lock region.
    pub in_region: bool,
    /// The requested surface and its native owner are alive and mapped.
    pub live: bool,
    /// Native acquisition has already succeeded for this owner.
    pub native_acquired: bool,
}

impl PointerLockEligibility {
    pub const fn can_activate(self) -> bool {
        self.focused && self.in_region && self.live && self.native_acquired
    }
}

#[cfg(test)]
mod tests {
    use super::{KeyTransition, KeyTransitionReducer, PointerLockEligibility, resume_focus};

    #[test]
    fn held_modifier_can_be_released_after_its_surface_loses_focus() {
        let mut keys = KeyTransitionReducer::<u64>::default();
        assert!(keys.set_owner(Some(1)).is_empty());
        assert!(keys.key_with_focus(1, 42, KeyTransition::Pressed, false, true));
        assert!(keys.key_with_focus(1, 42, KeyTransition::Released, false, false));
        assert!(keys.key_with_focus(1, 42, KeyTransition::Pressed, false, true));
        assert_eq!(keys.set_owner(None), vec![42]);
    }

    #[test]
    fn unfocused_presses_do_not_create_held_keys() {
        let mut keys = KeyTransitionReducer::<u64>::default();
        assert!(keys.set_owner(Some(1)).is_empty());
        assert!(!keys.key_with_focus(1, 42, KeyTransition::Pressed, false, false));
        assert!(!keys.key_with_focus(1, 42, KeyTransition::Released, false, false));
        assert!(!keys.key_with_focus(2, 42, KeyTransition::Released, false, false));
        assert!(keys.set_owner(None).is_empty());
    }

    #[test]
    fn reactivation_restores_only_a_live_suspended_target() {
        let mut saved = Some(7);
        assert_eq!(
            resume_focus(&mut saved, false, |target| *target == 7),
            Some(7)
        );
        assert!(saved.is_none());
        saved = Some(7);
        assert_eq!(resume_focus(&mut saved, false, |_| false), None);
        assert!(saved.is_none());
    }

    #[test]
    fn reactivation_does_not_override_new_or_deliberately_empty_focus() {
        let mut saved = Some(7);
        assert_eq!(
            resume_focus(&mut saved, true, |_| panic!("already focused")),
            None
        );
        assert!(saved.is_none());
        assert_eq!(resume_focus(&mut saved, false, |_| true), None);
    }
    use KeyTransition::{Pressed, Released};

    const A: u64 = 1;
    const B: u64 = 2;
    const KEY_A: u32 = 30;
    const KEY_B: u32 = 48;
    const LEFT_SHIFT: u32 = 42;

    #[test]
    fn unowned_keys_are_rejected_without_becoming_held() {
        let mut keys = KeyTransitionReducer::default();
        assert_eq!(keys.owner(), None);
        assert!(!keys.key(A, KEY_A, Pressed, false));
        assert!(!keys.key(A, KEY_A, Released, false));
        assert!(keys.set_owner(Some(A)).is_empty());
        assert!(!keys.key(A, KEY_A, Released, false));
        assert!(keys.focus_lost(A).is_empty());
    }

    #[test]
    fn repeated_presses_forward_one_press_and_one_release() {
        let mut keys = KeyTransitionReducer::default();
        assert!(keys.set_owner(Some(A)).is_empty());
        let mut forwarded = Vec::new();
        if keys.key(A, KEY_A, Pressed, false) {
            forwarded.push(Pressed);
        }
        for _ in 0..100 {
            if keys.key(A, KEY_A, Pressed, true) {
                forwarded.push(Pressed);
            }
        }
        if keys.key(A, KEY_A, Released, false) {
            forwarded.push(Released);
        }
        assert_eq!(forwarded, vec![Pressed, Released]);
        assert!(keys.focus_lost(A).is_empty());
    }

    #[test]
    fn repeat_without_initial_press_does_not_create_a_held_key() {
        let mut keys = KeyTransitionReducer::default();
        assert!(keys.set_owner(Some(A)).is_empty());
        assert!(!keys.key(A, KEY_A, Pressed, true));
        assert!(!keys.key(A, KEY_A, Released, false));
        assert!(keys.focus_lost(A).is_empty());
    }

    #[test]
    fn duplicate_presses_and_unmatched_releases_are_rejected() {
        let mut keys = KeyTransitionReducer::default();
        assert!(keys.set_owner(Some(A)).is_empty());
        assert!(!keys.key(A, KEY_A, Released, false));
        assert!(keys.key(A, KEY_A, Pressed, false));
        assert!(!keys.key(A, KEY_A, Pressed, false));
        assert!(keys.key(A, KEY_A, Released, false));
        assert!(!keys.key(A, KEY_A, Released, false));
        assert!(keys.key(A, KEY_A, Pressed, false));
        assert_eq!(keys.focus_lost(A), vec![KEY_A]);
    }

    #[test]
    fn repeat_flag_does_not_block_a_matching_release() {
        let mut keys = KeyTransitionReducer::default();
        assert!(keys.set_owner(Some(A)).is_empty());
        assert!(keys.key(A, KEY_A, Pressed, false));
        assert!(keys.key(A, KEY_A, Released, true));
        assert!(keys.focus_lost(A).is_empty());
    }

    #[test]
    fn focus_loss_returns_exact_held_evdev_keys_once() {
        let mut keys = KeyTransitionReducer::default();
        assert!(keys.set_owner(Some(A)).is_empty());
        assert!(keys.key(A, KEY_B, Pressed, false));
        assert!(keys.key(A, KEY_A, Pressed, false));
        assert!(keys.key(A, LEFT_SHIFT, Pressed, false));
        assert!(keys.key(A, KEY_A, Released, false));
        assert_eq!(keys.focus_lost(A), vec![LEFT_SHIFT, KEY_B]);
        assert_eq!(keys.owner(), None);
        assert!(keys.focus_lost(A).is_empty());
        assert!(!keys.key(A, KEY_B, Released, false));
    }

    #[test]
    fn stale_focus_loss_after_switch_does_not_release_new_owners_keys() {
        let mut keys = KeyTransitionReducer::default();
        assert!(keys.set_owner(Some(A)).is_empty());
        assert!(keys.key(A, KEY_A, Pressed, false));
        assert_eq!(keys.set_owner(Some(B)), vec![KEY_A]);
        assert!(keys.key(B, KEY_B, Pressed, false));
        assert!(keys.focus_lost(A).is_empty());
        assert_eq!(keys.owner(), Some(B));
        assert_eq!(keys.focus_lost(B), vec![KEY_B]);
    }

    #[test]
    fn non_owner_events_cannot_press_or_release_keys() {
        let mut keys = KeyTransitionReducer::default();
        assert!(keys.set_owner(Some(B)).is_empty());
        assert!(keys.key(B, KEY_A, Pressed, false));
        assert!(!keys.key(A, KEY_A, Released, false));
        assert!(!keys.key(A, KEY_B, Pressed, false));
        assert_eq!(keys.focus_lost(B), vec![KEY_A]);
    }

    #[test]
    fn reassignment_drains_old_keys_without_transferring_them() {
        let mut keys = KeyTransitionReducer::default();
        assert!(keys.set_owner(Some(A)).is_empty());
        assert!(keys.key(A, KEY_A, Pressed, false));
        assert!(keys.key(A, LEFT_SHIFT, Pressed, false));
        assert_eq!(keys.set_owner(Some(B)), vec![KEY_A, LEFT_SHIFT]);
        assert_eq!(keys.owner(), Some(B));
        assert!(!keys.key(A, KEY_A, Released, false));
        assert!(!keys.key(B, KEY_A, Released, false));
        assert!(!keys.key(B, KEY_A, Pressed, true));
        assert!(keys.key(B, KEY_A, Pressed, false));
        assert!(!keys.key(A, KEY_A, Released, false));
        assert!(keys.key(B, KEY_A, Released, false));
        assert!(keys.set_owner(Some(A)).is_empty());
        assert!(!keys.key(A, KEY_A, Released, false));
        assert!(keys.key(A, KEY_A, Pressed, false));
        assert_eq!(keys.focus_lost(A), vec![KEY_A]);
    }

    #[test]
    fn setting_same_owner_preserves_keys() {
        let mut keys = KeyTransitionReducer::default();
        assert!(keys.set_owner(Some(A)).is_empty());
        assert!(keys.key(A, KEY_A, Pressed, false));
        assert!(keys.set_owner(Some(A)).is_empty());
        assert!(!keys.key(A, KEY_A, Pressed, false));
        assert_eq!(keys.focus_lost(A), vec![KEY_A]);
    }

    #[test]
    fn clearing_owner_drains_keys_and_rejects_late_releases() {
        let mut keys = KeyTransitionReducer::default();
        assert!(keys.set_owner(Some(A)).is_empty());
        assert!(keys.key(A, KEY_A, Pressed, false));
        assert_eq!(keys.set_owner(None), vec![KEY_A]);
        assert!(keys.set_owner(None).is_empty());
        assert!(!keys.key(A, KEY_A, Released, false));
        assert!(keys.set_owner(Some(A)).is_empty());
        assert!(!keys.key(A, KEY_A, Released, false));
        assert!(keys.key(A, KEY_A, Pressed, false));
        assert!(keys.key(A, KEY_A, Released, false));
    }

    #[test]
    fn owner_does_not_need_to_implement_default() {
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        struct Window(u64);

        let mut keys = KeyTransitionReducer::<Window>::default();
        assert!(keys.set_owner(Some(Window(A))).is_empty());
        assert!(keys.key(Window(A), KEY_A, Pressed, false));
        assert!(keys.focus_lost(Window(B)).is_empty());
        assert_eq!(keys.focus_lost(Window(A)), vec![KEY_A]);
    }

    #[test]
    fn pointer_lock_requires_all_four_prerequisites() {
        for flags in 0..16 {
            let eligibility = PointerLockEligibility {
                focused: flags & 1 != 0,
                in_region: flags & 2 != 0,
                live: flags & 4 != 0,
                native_acquired: flags & 8 != 0,
            };
            assert_eq!(eligibility.can_activate(), flags == 15, "flags={flags}");
        }
        assert!(!PointerLockEligibility::default().can_activate());
    }
}
