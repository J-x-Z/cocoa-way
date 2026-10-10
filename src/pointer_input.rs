//! Pointer ownership and lock decisions without window, protocol, or hardware IO.

use std::collections::BTreeSet;

#[derive(Clone, Debug, PartialEq)]
pub struct PointerFocus<Owner, Surface> {
    pub owner: Owner,
    pub surface: Surface,
    pub origin: (f64, f64),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ConstraintKind {
    #[default]
    None,
    Locked,
    Confined,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ConstraintSnapshot {
    pub kind: ConstraintKind,
    pub active: bool,
    pub live: bool,
    pub in_region: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LockAction {
    None,
    Acquire,
    Release,
    ConfinementUnsupported,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LockStatus {
    #[default]
    Idle,
    Locked,
    ConfinementUnsupported,
    AcquisitionFailed,
    ReleaseFailed,
}

/// Native ownership survives protocol/focus destruction until release succeeds.
#[derive(Debug)]
pub struct PointerInput<Owner, Surface> {
    focus: Option<PointerFocus<Owner, Surface>>,
    native_grab: Option<PointerFocus<Owner, Surface>>,
    pending: bool,
    status: LockStatus,
    unsupported_confinement_requests: u64,
    held_buttons: BTreeSet<u32>,
}

impl<Owner, Surface> Default for PointerInput<Owner, Surface> {
    fn default() -> Self {
        Self {
            focus: None,
            native_grab: None,
            pending: false,
            status: LockStatus::Idle,
            unsupported_confinement_requests: 0,
            held_buttons: BTreeSet::new(),
        }
    }
}

impl<Owner: Copy + Eq, Surface: Clone + Eq> PointerInput<Owner, Surface> {
    pub fn focus(&self) -> Option<&PointerFocus<Owner, Surface>> {
        self.focus.as_ref()
    }

    pub fn native_grab(&self) -> Option<&PointerFocus<Owner, Surface>> {
        self.native_grab.as_ref()
    }

    pub fn status(&self) -> LockStatus {
        self.status
    }

    pub fn set_status(&mut self, status: LockStatus) -> bool {
        let changed = self.status != status;
        self.status = status;
        changed
    }

    pub fn unsupported_confinement_requests(&self) -> u64 {
        self.unsupported_confinement_requests
    }

    pub fn request_reconcile(&mut self) {
        self.pending = true;
    }

    pub fn constraint_created(&mut self, kind: ConstraintKind) {
        self.request_reconcile();
        if kind == ConstraintKind::Confined {
            self.unsupported_confinement_requests =
                self.unsupported_confinement_requests.saturating_add(1);
        }
    }

    /// Capture an accepted motion's origin, not a hit-test guess while a click
    /// grab keeps a different surface focused. Unknown origins fail closed.
    pub fn record_motion(
        &mut self,
        owner: Owner,
        candidate: Option<(Surface, (f64, f64))>,
        actual: Option<&Surface>,
    ) {
        self.request_reconcile();
        if let Some((surface, origin)) = candidate
            && actual == Some(&surface)
        {
            self.focus = Some(PointerFocus {
                owner,
                surface,
                origin,
            });
        } else if self.focus.as_ref().map(|focus| &focus.surface) != actual {
            self.focus = None;
        }
    }

    pub fn clear_focus(&mut self) {
        self.focus = None;
        self.request_reconcile();
    }

    /// Focus-enter can precede the old owner's focus-leave. Retire its click
    /// grab and held buttons before accepting input for the new native owner.
    pub fn needs_focus_reset(&self, next_owner: Owner) -> bool {
        self.focus
            .as_ref()
            .is_some_and(|focus| focus.owner != next_owner)
            || self
                .native_grab
                .as_ref()
                .is_some_and(|grab| grab.owner != next_owner)
    }

    pub fn forget_surface(&mut self, surface: &Surface) {
        if self
            .focus
            .as_ref()
            .is_some_and(|focus| &focus.surface == surface)
        {
            self.clear_focus();
        }
    }

    pub fn forget_window(&mut self, owner: Owner) {
        if self
            .focus
            .as_ref()
            .is_some_and(|focus| focus.owner == owner)
        {
            self.clear_focus();
        }
    }

    pub fn action(
        &self,
        focused_owner: Option<Owner>,
        actual: Option<&Surface>,
        constraint: ConstraintSnapshot,
    ) -> LockAction {
        let eligible = self.focus.as_ref().is_some_and(|focus| {
            focused_owner == Some(focus.owner)
                && actual == Some(&focus.surface)
                && constraint.live
                && constraint.in_region
        });
        if let Some(grab) = &self.native_grab {
            return if eligible
                && self.focus.as_ref() == Some(grab)
                && constraint.kind == ConstraintKind::Locked
                && constraint.active
                && self.status != LockStatus::ReleaseFailed
            {
                LockAction::None
            } else {
                LockAction::Release
            };
        }
        match constraint.kind {
            ConstraintKind::Confined => LockAction::ConfinementUnsupported,
            ConstraintKind::Locked
                if eligible && (self.pending || self.status != LockStatus::AcquisitionFailed) =>
            {
                LockAction::Acquire
            }
            _ => LockAction::None,
        }
    }

    /// Record native ownership before attempting protocol activation, so even
    /// failed activation can release or retain ownership for a release retry.
    pub fn native_acquired(&mut self, focus: PointerFocus<Owner, Surface>) {
        self.native_grab = Some(focus);
        self.pending = false;
        self.status = LockStatus::Idle;
    }

    pub fn activated(&mut self) {
        self.status = LockStatus::Locked;
    }

    pub fn acquisition_failed(&mut self) {
        self.pending = false;
        self.status = LockStatus::AcquisitionFailed;
    }

    /// A failed release must retain the owner so the next event can retry it.
    pub fn released(&mut self, succeeded: bool) {
        if succeeded {
            self.native_grab = None;
            self.pending = true;
            self.status = LockStatus::Idle;
        } else {
            self.status = LockStatus::ReleaseFailed;
        }
    }

    /// Disassociation affects the whole seat. Ignore even stale absolute events
    /// from another window until native ownership has been released.
    pub fn freezes_absolute(&self) -> bool {
        self.native_grab.is_some()
    }

    pub fn relative_focus(
        &self,
        focused_owner: Option<Owner>,
        actual: Option<&Surface>,
    ) -> Option<&PointerFocus<Owner, Surface>> {
        let focus = self.focus.as_ref()?;
        (focused_owner == Some(focus.owner)
            && actual == Some(&focus.surface)
            && (self.native_grab.is_none() || self.status == LockStatus::Locked))
            .then_some(focus)
    }

    pub fn button(&mut self, button: u32, pressed: bool) {
        if pressed {
            self.held_buttons.insert(button);
        } else {
            self.held_buttons.remove(&button);
        }
    }

    pub fn take_buttons(&mut self) -> Vec<u32> {
        std::mem::take(&mut self.held_buttons).into_iter().collect()
    }
}

pub fn inside_surface(local: (f64, f64), size: (f64, f64)) -> bool {
    local.0.is_finite()
        && local.1.is_finite()
        && size.0.is_finite()
        && size.1.is_finite()
        && local.0 >= 0.0
        && local.1 >= 0.0
        && local.0 < size.0
        && local.1 < size.1
}

/// Winit's macOS deltas come from NSEvent, not verified raw hardware counts.
/// This converts host point deltas to guest logical units without clipping;
/// it does not recover independent, sensitivity-free unaccelerated motion.
pub fn relative_delta(delta: (f64, f64), points_per_logical: f64) -> Option<(f64, f64)> {
    if !delta.0.is_finite()
        || !delta.1.is_finite()
        || !points_per_logical.is_finite()
        || points_per_logical <= 0.0
    {
        return None;
    }
    let result = (delta.0 / points_per_logical, delta.1 / points_per_logical);
    (result.0.is_finite() && result.1.is_finite() && result != (0.0, 0.0)).then_some(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn focused() -> PointerInput<u64, u64> {
        let mut input = PointerInput::default();
        input.record_motion(1, Some((10, (30.0, 40.0))), Some(&10));
        input
    }

    fn lock(active: bool) -> ConstraintSnapshot {
        ConstraintSnapshot {
            kind: ConstraintKind::Locked,
            active,
            live: true,
            in_region: true,
        }
    }

    #[test]
    fn lock_requires_actual_focus_owner_mapping_and_region() {
        let input = focused();
        assert_eq!(
            input.action(Some(1), Some(&10), lock(false)),
            LockAction::Acquire
        );
        assert_eq!(
            input.action(Some(2), Some(&10), lock(false)),
            LockAction::None
        );
        assert_eq!(
            input.action(Some(1), Some(&20), lock(false)),
            LockAction::None
        );
        assert_eq!(input.action(None, Some(&10), lock(false)), LockAction::None);
        for constraint in [
            ConstraintSnapshot {
                live: false,
                ..lock(false)
            },
            ConstraintSnapshot {
                in_region: false,
                ..lock(false)
            },
        ] {
            assert_eq!(
                input.action(Some(1), Some(&10), constraint),
                LockAction::None
            );
        }
    }

    #[test]
    fn acquisition_failure_requires_a_new_request_or_motion_to_retry() {
        let mut input = focused();
        input.acquisition_failed();
        assert!(input.native_grab().is_none());
        assert_eq!(
            input.action(Some(1), Some(&10), lock(false)),
            LockAction::None
        );
        input.constraint_created(ConstraintKind::Locked);
        assert_eq!(
            input.action(Some(1), Some(&10), lock(false)),
            LockAction::Acquire
        );
    }

    #[test]
    fn active_lock_freezes_absolute_and_keeps_relative_focus() {
        let mut input = focused();
        input.native_acquired(input.focus().unwrap().clone());
        input.activated();
        assert!(input.freezes_absolute());
        assert_eq!(
            input.relative_focus(Some(1), Some(&10)).unwrap().origin,
            (30.0, 40.0)
        );
        assert!(input.relative_focus(Some(2), Some(&10)).is_none());
        assert_eq!(
            input.action(Some(1), Some(&10), lock(true)),
            LockAction::None
        );
    }

    #[test]
    fn focus_or_constraint_loss_releases_native_grab() {
        let mut input = focused();
        input.native_acquired(input.focus().unwrap().clone());
        input.activated();
        assert_eq!(
            input.action(None, Some(&10), lock(true)),
            LockAction::Release
        );
        assert_eq!(
            input.action(Some(1), Some(&10), ConstraintSnapshot::default()),
            LockAction::Release
        );
        assert_eq!(
            input.action(Some(1), Some(&10), lock(false)),
            LockAction::Release
        );
        input.forget_surface(&10);
        assert!(input.focus().is_none());
        assert!(input.native_grab().is_some());
        assert_eq!(input.action(Some(1), None, lock(true)), LockAction::Release);
    }

    #[test]
    fn failed_release_keeps_owner_and_blocks_relative_input() {
        let mut input = focused();
        input.native_acquired(input.focus().unwrap().clone());
        input.activated();
        input.released(false);
        assert_eq!(input.native_grab().unwrap().owner, 1);
        assert!(input.relative_focus(Some(1), Some(&10)).is_none());
        assert_eq!(input.action(None, None, lock(false)), LockAction::Release);
        input.released(true);
        assert!(input.native_grab().is_none());
    }

    #[test]
    fn stale_window_loss_does_not_forget_another_focus() {
        let mut input = focused();
        input.record_motion(2, Some((20, (0.0, 0.0))), Some(&20));
        input.forget_window(1);
        assert_eq!(input.focus().unwrap().owner, 2);
        input.forget_window(2);
        assert!(input.focus().is_none());
    }

    #[test]
    fn focus_enter_before_old_leave_retires_only_previous_owner() {
        let mut input = focused();
        input.button(0x110, true);
        assert!(!input.needs_focus_reset(1));
        assert!(input.needs_focus_reset(2));
        assert_eq!(input.take_buttons(), vec![0x110]);
        input.clear_focus();
        input.record_motion(2, Some((20, (0.0, 0.0))), Some(&20));
        input.button(0x111, true);
        input.forget_window(1);
        assert!(!input.needs_focus_reset(2));
        assert_eq!(input.focus().unwrap().owner, 2);
        assert_eq!(input.take_buttons(), vec![0x111]);
    }

    #[test]
    fn focus_switch_retains_failed_native_release_until_retry_succeeds() {
        let mut input = focused();
        input.native_acquired(input.focus().unwrap().clone());
        input.activated();
        input.clear_focus();
        input.released(false);
        assert!(input.needs_focus_reset(2));
        assert!(!input.needs_focus_reset(1));
        assert!(input.freezes_absolute());
        assert_eq!(
            input.action(Some(2), None, lock(false)),
            LockAction::Release
        );
        input.released(true);
        assert!(!input.needs_focus_reset(2));
        assert!(!input.freezes_absolute());
    }

    #[test]
    fn click_grab_preserves_actual_origin_instead_of_hit_test_guess() {
        let mut input = focused();
        input.record_motion(2, Some((20, (100.0, 200.0))), Some(&10));
        assert_eq!(input.focus().unwrap().owner, 1);
        assert_eq!(input.focus().unwrap().origin, (30.0, 40.0));
        input.record_motion(2, Some((20, (100.0, 200.0))), Some(&99));
        assert!(input.focus().is_none());
    }

    #[test]
    fn confinement_is_recorded_and_never_acquired() {
        let mut input = focused();
        input.constraint_created(ConstraintKind::Confined);
        assert_eq!(input.unsupported_confinement_requests(), 1);
        let confined = ConstraintSnapshot {
            kind: ConstraintKind::Confined,
            ..lock(false)
        };
        assert_eq!(
            input.action(Some(1), Some(&10), confined),
            LockAction::ConfinementUnsupported
        );
        assert!(input.native_grab().is_none());
    }

    #[test]
    fn relative_deltas_are_unclipped_and_scaled_once() {
        assert_eq!(relative_delta((7.0, -3.0), 1.0), Some((7.0, -3.0)));
        assert_eq!(relative_delta((7.0, -3.0), 0.5), Some((14.0, -6.0)));
        assert_eq!(relative_delta((0.0, 0.0), 1.0), None);
        for scale in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert_eq!(relative_delta((7.0, -3.0), scale), None);
        }
        assert_eq!(relative_delta((f64::NAN, 1.0), 1.0), None);
    }

    #[test]
    fn button_cleanup_is_exact_and_drained_once() {
        let mut input = focused();
        input.button(0x111, true);
        input.button(0x110, true);
        input.button(0x110, true);
        input.button(0x111, false);
        assert_eq!(input.take_buttons(), vec![0x110]);
        assert!(input.take_buttons().is_empty());
    }

    #[test]
    fn native_acquisition_does_not_forward_before_protocol_activation() {
        let mut input = focused();
        input.native_acquired(input.focus().unwrap().clone());
        assert!(input.freezes_absolute());
        assert!(input.relative_focus(Some(1), Some(&10)).is_none());
        input.activated();
        assert!(input.relative_focus(Some(1), Some(&10)).is_some());
    }

    #[test]
    fn replacement_constraint_releases_before_reacquiring() {
        let mut input = focused();
        input.native_acquired(input.focus().unwrap().clone());
        input.activated();
        input.constraint_created(ConstraintKind::Locked);
        assert_eq!(
            input.action(Some(1), Some(&10), lock(false)),
            LockAction::Release
        );
        input.released(true);
        assert!(!input.freezes_absolute());
        assert_eq!(
            input.action(Some(1), Some(&10), lock(false)),
            LockAction::Acquire
        );
    }

    #[test]
    fn lost_mapping_or_region_deactivates_an_existing_lock() {
        let mut input = focused();
        input.native_acquired(input.focus().unwrap().clone());
        input.activated();
        for constraint in [
            ConstraintSnapshot {
                live: false,
                ..lock(true)
            },
            ConstraintSnapshot {
                in_region: false,
                ..lock(true)
            },
        ] {
            assert_eq!(
                input.action(Some(1), Some(&10), constraint),
                LockAction::Release
            );
        }
    }

    #[test]
    fn surface_bounds_are_finite_fractional_and_half_open() {
        assert!(inside_surface((0.0, 0.0), (10.5, 20.5)));
        assert!(inside_surface((10.25, 20.25), (10.5, 20.5)));
        assert!(!inside_surface((10.5, 1.0), (10.5, 20.5)));
        assert!(!inside_surface((1.0, 20.5), (10.5, 20.5)));
        assert!(!inside_surface((-0.25, 1.0), (10.5, 20.5)));
        assert!(!inside_surface((1.0, f64::NAN), (10.5, 20.5)));
        assert!(!inside_surface((0.0, 0.0), (0.0, 20.5)));
        assert!(!inside_surface((1.0, 1.0), (f64::INFINITY, 20.5)));
    }
}

#[cfg(test)]
mod seat_tests {
    use super::{ConstraintKind, ConstraintSnapshot, LockAction, PointerInput};
    use smithay::input::pointer::*;
    use smithay::input::{Seat, SeatHandler, SeatState};
    use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
    use smithay::utils::{IsAlive, SERIAL_COUNTER, Serial};

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct Target(u64);

    #[derive(Debug, PartialEq)]
    enum Delivered {
        Enter(u64, (f64, f64)),
        Motion(u64, (f64, f64)),
        Relative(u64, (f64, f64), u64),
        Leave(u64),
    }

    struct TestSeat {
        seats: SeatState<Self>,
        delivered: Vec<Delivered>,
    }

    impl SeatHandler for TestSeat {
        type KeyboardFocus = WlSurface;
        type PointerFocus = Target;
        type TouchFocus = WlSurface;

        fn seat_state(&mut self) -> &mut SeatState<Self> {
            &mut self.seats
        }
    }

    impl IsAlive for Target {
        fn alive(&self) -> bool {
            true
        }
    }

    macro_rules! ignore_gestures {
        ($($name:ident: $event:ty),* $(,)?) => {
            $(fn $name(&self, _: &Seat<TestSeat>, _: &mut TestSeat, _: &$event) {})*
        };
    }

    impl PointerTarget<TestSeat> for Target {
        fn enter(&self, _: &Seat<TestSeat>, state: &mut TestSeat, event: &MotionEvent) {
            state.delivered.push(Delivered::Enter(
                self.0,
                (event.location.x, event.location.y),
            ));
        }

        fn motion(&self, _: &Seat<TestSeat>, state: &mut TestSeat, event: &MotionEvent) {
            state.delivered.push(Delivered::Motion(
                self.0,
                (event.location.x, event.location.y),
            ));
        }

        fn relative_motion(
            &self,
            _: &Seat<TestSeat>,
            state: &mut TestSeat,
            event: &RelativeMotionEvent,
        ) {
            state.delivered.push(Delivered::Relative(
                self.0,
                (event.delta.x, event.delta.y),
                event.utime,
            ));
        }

        fn leave(&self, _: &Seat<TestSeat>, state: &mut TestSeat, _: Serial, _: u32) {
            state.delivered.push(Delivered::Leave(self.0));
        }

        fn button(&self, _: &Seat<TestSeat>, _: &mut TestSeat, _: &ButtonEvent) {}
        fn axis(&self, _: &Seat<TestSeat>, _: &mut TestSeat, _: AxisFrame) {}
        fn frame(&self, _: &Seat<TestSeat>, _: &mut TestSeat) {}

        ignore_gestures! {
            gesture_swipe_begin: GestureSwipeBeginEvent,
            gesture_swipe_update: GestureSwipeUpdateEvent,
            gesture_swipe_end: GestureSwipeEndEvent,
            gesture_pinch_begin: GesturePinchBeginEvent,
            gesture_pinch_update: GesturePinchUpdateEvent,
            gesture_pinch_end: GesturePinchEndEvent,
            gesture_hold_begin: GestureHoldBeginEvent,
            gesture_hold_end: GestureHoldEndEvent,
        }
    }

    fn seat() -> (TestSeat, PointerHandle<TestSeat>) {
        let mut seats = SeatState::new();
        let mut seat = seats.new_seat("in-process-pointer-test");
        let pointer = seat.add_pointer();
        (
            TestSeat {
                seats,
                delivered: Vec::new(),
            },
            pointer,
        )
    }

    fn motion(location: (f64, f64)) -> MotionEvent {
        MotionEvent {
            location: location.into(),
            serial: SERIAL_COUNTER.next_serial(),
            time: 1,
        }
    }

    #[test]
    fn locked_seat_delivers_one_relative_sample_without_moving_or_retargeting() {
        let (mut state, pointer) = seat();
        let mut input = PointerInput::<u64, Target>::default();
        pointer.motion(
            &mut state,
            Some((Target(10), (30.0, 40.0).into())),
            &motion((35.0, 47.0)),
        );
        input.record_motion(
            1,
            Some((Target(10), (30.0, 40.0))),
            pointer.current_focus().as_ref(),
        );
        input.native_acquired(input.focus().unwrap().clone());
        input.activated();
        if !input.freezes_absolute() {
            pointer.motion(
                &mut state,
                Some((Target(20), (0.0, 0.0).into())),
                &motion((500.0, 500.0)),
            );
        }
        let focus = input
            .relative_focus(Some(1), pointer.current_focus().as_ref())
            .unwrap();
        pointer.relative_motion(
            &mut state,
            Some((focus.surface.clone(), focus.origin.into())),
            &RelativeMotionEvent {
                delta: (7.0, -3.0).into(),
                delta_unaccel: (7.0, -3.0).into(),
                utime: 123_456,
            },
        );
        pointer.frame(&mut state);
        assert_eq!(pointer.current_focus(), Some(Target(10)));
        assert_eq!(pointer.current_location(), (35.0, 47.0).into());
        assert_eq!(
            state.delivered,
            vec![
                Delivered::Enter(10, (5.0, 7.0)),
                Delivered::Relative(10, (7.0, -3.0), 123_456)
            ]
        );

        assert_eq!(
            input.action(
                None,
                pointer.current_focus().as_ref(),
                ConstraintSnapshot {
                    kind: ConstraintKind::Locked,
                    active: true,
                    live: true,
                    in_region: true,
                }
            ),
            LockAction::Release
        );
        input.released(true);
        pointer.motion(&mut state, None, &motion((35.0, 47.0)));
        input.clear_focus();
        assert!(pointer.current_focus().is_none());
        assert!(
            input
                .relative_focus(Some(1), pointer.current_focus().as_ref())
                .is_none()
        );
        assert_eq!(state.delivered.last(), Some(&Delivered::Leave(10)));
    }

    #[test]
    fn smithay_click_grab_retains_recorded_owner_and_origin() {
        let (mut state, pointer) = seat();
        let mut input = PointerInput::<u64, Target>::default();
        pointer.motion(
            &mut state,
            Some((Target(10), (30.0, 40.0).into())),
            &motion((35.0, 47.0)),
        );
        input.record_motion(
            1,
            Some((Target(10), (30.0, 40.0))),
            pointer.current_focus().as_ref(),
        );
        pointer.button(
            &mut state,
            &ButtonEvent {
                button: 0x110,
                state: smithay::backend::input::ButtonState::Pressed,
                serial: SERIAL_COUNTER.next_serial(),
                time: 2,
            },
        );
        pointer.motion(
            &mut state,
            Some((Target(20), (100.0, 200.0).into())),
            &motion((105.0, 207.0)),
        );
        input.record_motion(
            2,
            Some((Target(20), (100.0, 200.0))),
            pointer.current_focus().as_ref(),
        );
        assert_eq!(pointer.current_focus(), Some(Target(10)));
        assert_eq!(input.focus().unwrap().owner, 1);
        assert_eq!(input.focus().unwrap().origin, (30.0, 40.0));
        assert_eq!(
            state.delivered.last(),
            Some(&Delivered::Motion(10, (75.0, 167.0)))
        );
        pointer.motion(
            &mut state,
            Some((Target(10), (100.0, 200.0).into())),
            &motion((105.0, 207.0)),
        );
        let (surface, origin) = pointer.grab_start_data().unwrap().focus.unwrap();
        assert_eq!(origin, (30.0, 40.0).into());
        input.record_motion(
            input.focus().unwrap().owner,
            Some((surface, (origin.x, origin.y))),
            pointer.current_focus().as_ref(),
        );
        assert_eq!(input.focus().unwrap().origin, (30.0, 40.0));
    }
}
