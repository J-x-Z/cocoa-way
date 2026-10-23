//! Pure input policy; callers own key mapping, forwarding, focus, and native grabs.
//!
//! Key codes are evdev codes without the XKB +8 offset. Release drained keys to
//! the previous focus before applying an owner change to the actual keyboard.

use std::collections::BTreeSet;
use std::time::Instant;

use winit::event::{Event, MouseButton};

/// Winit emits device precursors and NewEvents before native window input.
/// UserEvent must not drain/replenish control work inside Winit's proxy drain.
pub fn admit_control_work<T: 'static>(
    event: &Event<T>,
    budget: &mut crate::event_turn::ControlWorkBudget,
    now: Instant,
) -> bool {
    matches!(event, Event::AboutToWait) && budget.admit(now)
}

/// NSEvent's Other button numbers are not Linux evdev codes.
pub fn evdev_mouse_button(button: MouseButton) -> Option<u32> {
    match button {
        MouseButton::Left => Some(0x110),
        MouseButton::Right => Some(0x111),
        MouseButton::Middle => Some(0x112),
        MouseButton::Back => Some(0x116),
        MouseButton::Forward => Some(0x115),
        MouseButton::Other(_) => None,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyTransition {
    Pressed,
    Released,
}

/// Only explicit paste presses need a fresh host snapshot on the input path.
/// Ordinary typing and releases must never realize an image pasteboard provider.
pub fn requests_clipboard_refresh(
    evdev_key: u32,
    transition: KeyTransition,
    repeat: bool,
    ctrl: bool,
    logo: bool,
    shift: bool,
) -> bool {
    transition == KeyTransition::Pressed
        && !repeat
        && ((evdev_key == 47 && (ctrl || logo)) || (evdev_key == 110 && shift))
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
    fn native_precursors_and_proxy_events_leave_control_work_for_about_to_wait() {
        use super::admit_control_work;
        use crate::event_turn::ControlWorkBudget;
        use std::time::Instant;
        use winit::event::{
            DeviceEvent, DeviceId, ElementState, Event, MouseButton, MouseScrollDelta, StartCause,
            TouchPhase, WindowEvent,
        };

        let (sender, receiver) = crate::event_loop_signal::channel_with_waker(|| {});
        sender.send(1).unwrap();
        sender.send(2).unwrap();
        let now = Instant::now();
        let device_id = DeviceId::dummy();
        let window_id = winit::window::WindowId::from(1_u64);
        let device = |event| Event::<()>::DeviceEvent { device_id, event };
        let window = |event| Event::<()>::WindowEvent { window_id, event };
        let delta = MouseScrollDelta::LineDelta(0.0, 1.0);
        // Preserve macOS's axis -> relative -> absolute, device -> button, and
        // device-wheel -> window-wheel ordering, plus the surrounding loop events.
        let events = [
            Event::NewEvents(StartCause::WaitCancelled {
                start: now,
                requested_resume: None,
            }),
            window(WindowEvent::CursorEntered { device_id }),
            device(DeviceEvent::Motion {
                axis: 0,
                value: 2.0,
            }),
            device(DeviceEvent::Motion {
                axis: 1,
                value: 3.0,
            }),
            device(DeviceEvent::MouseMotion { delta: (2.0, 3.0) }),
            window(WindowEvent::CursorMoved {
                device_id,
                position: (20.0, 30.0).into(),
            }),
            device(DeviceEvent::Button {
                button: 0,
                state: ElementState::Pressed,
            }),
            window(WindowEvent::MouseInput {
                device_id,
                button: MouseButton::Left,
                state: ElementState::Pressed,
            }),
            window(WindowEvent::ModifiersChanged(Default::default())),
            device(DeviceEvent::MouseWheel { delta }),
            window(WindowEvent::MouseWheel {
                device_id,
                delta,
                phase: TouchPhase::Moved,
            }),
            Event::UserEvent(()),
            window(WindowEvent::RedrawRequested),
            window(WindowEvent::Focused(false)),
            window(WindowEvent::CursorLeft { device_id }),
            window(WindowEvent::CloseRequested),
            window(WindowEvent::Destroyed),
            Event::LoopExiting,
        ];
        // Use an otherwise-admitting budget: event policy, not the old input
        // classification, must prevent each precursor from consuming the queue.
        for event in events {
            let mut budget = ControlWorkBudget::new(false, now);
            assert!(!admit_control_work(&event, &mut budget, now), "{event:?}");
        }
        let mut budget = ControlWorkBudget::new(false, now);
        let mut received = Vec::new();
        while admit_control_work(&Event::<()>::AboutToWait, &mut budget, now) {
            let Ok(message) = receiver.try_recv() else {
                break;
            };
            sender.message_received();
            received.push(message);
        }
        assert_eq!(received, [1, 2]);
        assert!(!sender.finish_turn());
    }

    #[test]
    fn control_backlog_yields_without_replenishing_the_proxy_drain() {
        use super::admit_control_work;
        use crate::event_turn::ControlWorkBudget;
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        use std::time::Instant;
        use winit::event::{DeviceEvent, DeviceId, Event, StartCause, WindowEvent};

        let wakes = Arc::new(AtomicUsize::new(0));
        let counted_wakes = wakes.clone();
        let (sender, receiver) = crate::event_loop_signal::channel_with_waker(move || {
            counted_wakes.fetch_add(1, Ordering::SeqCst);
        });
        for message in 0..130 {
            sender.send(message).unwrap();
        }
        let now = Instant::now();
        let mut received = Vec::new();
        for expected in [64, 128, 131] {
            // A backlog must not turn repeated proxy events into control turns.
            for _ in 0..64 {
                let mut budget = ControlWorkBudget::new(false, now);
                assert!(!admit_control_work(&Event::UserEvent(()), &mut budget, now));
            }
            for event in [
                Event::NewEvents(StartCause::Poll),
                Event::DeviceEvent {
                    device_id: DeviceId::dummy(),
                    event: DeviceEvent::MouseMotion { delta: (1.0, 0.0) },
                },
                Event::WindowEvent {
                    window_id: winit::window::WindowId::from(1_u64),
                    event: WindowEvent::RedrawRequested,
                },
            ] {
                let mut budget = ControlWorkBudget::new(false, now);
                assert!(!admit_control_work::<()>(&event, &mut budget, now));
            }
            let mut budget = ControlWorkBudget::new(false, now);
            while admit_control_work(&Event::<()>::AboutToWait, &mut budget, now) {
                let Ok(message) = receiver.try_recv() else {
                    break;
                };
                sender.message_received();
                received.push(message);
                if message == 0 {
                    sender.send(130).unwrap();
                }
            }
            assert_eq!(received.len(), expected);
            assert_eq!(sender.finish_turn(), expected < 131);
            assert_eq!(wakes.load(Ordering::SeqCst), 1);
        }
        assert_eq!(received, (0..131).collect::<Vec<_>>());
        // Once idle, fresh work still wakes the native loop.
        sender.send(131).unwrap();
        assert_eq!(wakes.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn maintenance_time_budget_preserves_queued_work_for_the_next_turn() {
        use super::admit_control_work;
        use crate::event_turn::ControlWorkBudget;
        use std::time::{Duration, Instant};
        use winit::event::Event;

        let (sender, receiver) = crate::event_loop_signal::channel_with_waker(|| {});
        sender.send(1).unwrap();
        sender.send(2).unwrap();
        let event = Event::<()>::AboutToWait;
        let now = Instant::now();
        let mut budget = ControlWorkBudget::new(false, now);
        assert!(admit_control_work(&event, &mut budget, now));
        assert_eq!(receiver.try_recv().unwrap(), 1);
        sender.message_received();
        let later = now + Duration::from_millis(2);
        assert!(!admit_control_work(&event, &mut budget, later));
        assert!(sender.finish_turn());
        let mut next_budget = ControlWorkBudget::new(false, later);
        assert!(admit_control_work(&event, &mut next_budget, later));
        assert_eq!(receiver.try_recv().unwrap(), 2);
        sender.message_received();
        assert!(!sender.finish_turn());
    }

    #[test]
    fn mouse_buttons_use_explicit_evdev_codes() {
        use super::evdev_mouse_button;
        use winit::event::MouseButton::*;

        for (button, expected) in [
            (Left, 0x110),
            (Right, 0x111),
            (Middle, 0x112),
            (Back, 0x116),
            (Forward, 0x115),
        ] {
            assert_eq!(evdev_mouse_button(button), Some(expected));
        }
        for code in [0, 5, 0x110, u16::MAX] {
            assert_eq!(evdev_mouse_button(Other(code)), None);
        }
    }

    #[test]
    fn side_button_release_cannot_release_a_held_left_button() {
        use super::evdev_mouse_button;
        use winit::event::MouseButton::*;

        for side in [Back, Forward] {
            let sequence = [(Left, true), (side, true), (side, false), (Left, false)];
            for (length, expected) in [(3, vec![0x110]), (4, vec![])] {
                let mut pointer = crate::pointer_input::PointerInput::<u64, u64>::default();
                let mut delivered = Vec::new();
                for &(button, pressed) in &sequence[..length] {
                    let code = evdev_mouse_button(button).unwrap();
                    pointer.button(code, pressed);
                    delivered.push((code, pressed));
                }
                assert_eq!(delivered[2], (evdev_mouse_button(side).unwrap(), false));
                assert_eq!(pointer.take_buttons(), expected);
            }
        }
    }

    #[test]
    fn clipboard_refresh_is_reserved_for_paste_not_ordinary_key_transitions() {
        use super::requests_clipboard_refresh as refresh;
        assert!(refresh(47, Pressed, false, true, false, false));
        assert!(refresh(47, Pressed, false, true, false, true));
        assert!(refresh(47, Pressed, false, false, true, false));
        assert!(refresh(110, Pressed, false, false, false, true));
        assert!(!refresh(47, Released, false, true, false, true));
        assert!(!refresh(47, Pressed, true, true, false, true));
        assert!(!refresh(47, Pressed, false, false, false, false));
        assert!(!refresh(30, Pressed, false, true, false, true));
        assert!(!refresh(110, Pressed, false, false, false, false));
    }

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
