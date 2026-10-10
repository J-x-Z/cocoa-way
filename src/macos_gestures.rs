use block2::RcBlock;
use objc2::runtime::AnyObject;
use objc2_app_kit::{NSEvent, NSEventMask, NSEventPhase, NSEventType};
use std::{
    cell::RefCell,
    collections::{HashSet, VecDeque},
    ptr::NonNull,
    sync::OnceLock,
};
use winit::{event::TouchPhase, event_loop::EventLoopProxy, window::Window};

#[derive(Clone, Copy, Debug)]
pub struct SwipeEvent {
    pub window_number: isize,
    pub delta: (f64, f64),
    pub phase: TouchPhase,
}

struct MonitorState {
    windows: HashSet<isize>,
    monitor: Option<objc2::rc::Retained<AnyObject>>,
}

thread_local! {
    static PENDING: RefCell<VecDeque<SwipeEvent>> = const { RefCell::new(VecDeque::new()) };
    static MONITOR: RefCell<MonitorState> = RefCell::new(MonitorState {
        windows: HashSet::new(),
        monitor: None,
    });
}

static EVENT_LOOP_PROXY: OnceLock<EventLoopProxy<()>> = OnceLock::new();

pub fn set_event_loop_proxy(proxy: EventLoopProxy<()>) {
    let _ = EVENT_LOOP_PROXY.set(proxy);
}

fn queue(event: SwipeEvent) {
    PENDING.with(|pending| pending.borrow_mut().push_back(event));
    if let Some(proxy) = EVENT_LOOP_PROXY.get() {
        let _ = proxy.send_event(());
    }
}

fn queue_discrete_swipe(window_number: isize, delta: (f64, f64)) {
    if delta.0 == 0.0 && delta.1 == 0.0 {
        return;
    }
    queue(SwipeEvent {
        window_number,
        delta: (0.0, 0.0),
        phase: TouchPhase::Started,
    });
    queue(SwipeEvent {
        window_number,
        delta,
        phase: TouchPhase::Moved,
    });
    queue(SwipeEvent {
        window_number,
        delta: (0.0, 0.0),
        phase: TouchPhase::Ended,
    });
}

pub fn drain_swipe_events() -> Vec<SwipeEvent> {
    PENDING.with(|pending| pending.borrow_mut().drain(..).collect())
}

pub fn window_number(window: &Window) -> Option<isize> {
    use objc2::msg_send;
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    let handle = window.window_handle().ok()?;
    let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
        return None;
    };
    let ns_view = handle.ns_view.as_ptr() as *mut AnyObject;
    let ns_window: *mut AnyObject = unsafe { msg_send![ns_view, window] };
    if ns_window.is_null() {
        return None;
    }
    Some(unsafe { msg_send![ns_window, windowNumber] })
}

fn install_monitor() -> Result<objc2::rc::Retained<AnyObject>, String> {
    let block = RcBlock::new(|event: NonNull<NSEvent>| -> *mut NSEvent {
        let event_ref = unsafe { event.as_ref() };
        let window_number = unsafe { event_ref.windowNumber() };
        let tracked = MONITOR.with(|state| state.borrow().windows.contains(&window_number));
        if unsafe { event_ref.r#type() } == NSEventType::ScrollWheel {
            if suppress_scroll_momentum(
                tracked,
                unsafe { event_ref.hasPreciseScrollingDeltas() },
                unsafe { event_ref.momentumPhase() },
            ) {
                // Finger/axis_stop lets the guest own kinetic scrolling. Forwarding
                // AppKit's post-release momentum as well would apply inertia twice.
                return std::ptr::null_mut();
            }
        } else if tracked {
            let delta = unsafe { (event_ref.deltaX(), -event_ref.deltaY()) };
            queue_discrete_swipe(window_number, delta);
        }
        event.as_ptr()
    });
    unsafe {
        NSEvent::addLocalMonitorForEventsMatchingMask_handler(
            NSEventMask::Swipe | NSEventMask::ScrollWheel,
            &block,
        )
    }
    .ok_or_else(|| "macOS did not install the local gesture event monitor".into())
}

fn suppress_scroll_momentum(tracked: bool, precise: bool, momentum: NSEventPhase) -> bool {
    tracked && precise && momentum != NSEventPhase::None
}

// Keep the old function name so the event-loop integration stays platform-local.
// The monitor leaves raw scroll, magnify and rotation to Winit, and filters only
// synthesized scroll momentum in registered guest windows, not the GUI panel.
pub fn install_swipe_recognizer(window: &Window) -> Result<(), String> {
    let window_number = window_number(window).ok_or("AppKit window is not attached yet")?;
    MONITOR.with(|state| {
        let mut state = state.borrow_mut();
        state.windows.insert(window_number);
        if state.monitor.is_none() {
            state.monitor = Some(install_monitor()?);
        }
        Ok(())
    })
}

pub fn uninstall_swipe_recognizer(window_number: isize) {
    MONITOR.with(|state| {
        let mut state = state.borrow_mut();
        state.windows.remove(&window_number);
        if state.windows.is_empty()
            && let Some(monitor) = state.monitor.take()
        {
            unsafe { NSEvent::removeMonitor(&monitor) };
        }
    });
}

pub fn uninstall_all_swipe_recognizers() {
    MONITOR.with(|state| {
        let mut state = state.borrow_mut();
        state.windows.clear();
        if let Some(monitor) = state.monitor.take() {
            unsafe { NSEvent::removeMonitor(&monitor) };
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_finger_scrolls_and_mouse_wheels_reach_winit() {
        assert!(!suppress_scroll_momentum(true, true, NSEventPhase::None));
        assert!(!suppress_scroll_momentum(
            true,
            false,
            NSEventPhase::Changed
        ));
    }

    #[test]
    fn guest_windows_drop_every_host_momentum_phase() {
        for phase in [
            NSEventPhase::Began,
            NSEventPhase::Changed,
            NSEventPhase::Ended,
            NSEventPhase::Cancelled,
        ] {
            assert!(suppress_scroll_momentum(true, true, phase));
        }
    }

    #[test]
    fn gui_panel_keeps_native_scroll_momentum() {
        assert!(!suppress_scroll_momentum(
            false,
            true,
            NSEventPhase::Changed
        ));
    }
}
