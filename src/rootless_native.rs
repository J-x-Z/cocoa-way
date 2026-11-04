use objc2::rc::Retained;
use objc2_app_kit::{NSView, NSWindow, NSWindowOrderingMode};
use objc2_foundation::{MainThreadMarker, NSPoint};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

fn ns_window(window: &winit::window::Window) -> Option<Retained<NSWindow>> {
    MainThreadMarker::new()?;
    let RawWindowHandle::AppKit(handle) = window.window_handle().ok()?.as_raw() else {
        return None;
    };
    // The winit handle is a live main-thread content view.
    let view = unsafe { &*(handle.ns_view.as_ptr() as *const NSView) };
    view.window()
}

pub(crate) fn attach_child_window(parent: &winit::window::Window, child: &winit::window::Window) {
    if let (Some(parent), Some(child)) = (ns_window(parent), ns_window(child)) {
        attach_native_child(&parent, &child);
    }
}

pub(crate) fn detach_window(window: &winit::window::Window) {
    if let Some(window) = ns_window(window) {
        detach_native_window(&window);
    }
}

pub(crate) fn centered_child_origin(
    parent_origin: (f64, f64),
    parent_size: (f64, f64),
    child_size: (f64, f64),
) -> (f64, f64) {
    (
        parent_origin.0 + (parent_size.0 - child_size.0) / 2.0,
        parent_origin.1 + (parent_size.1 - child_size.1) / 2.0,
    )
}

pub(crate) fn attach_native_child(parent: &NSWindow, child: &NSWindow) {
    if std::ptr::eq(parent, child) {
        return;
    }
    let parent_frame = parent.frame();
    let mut frame = child.frame();
    // Both frames use global AppKit points, not differently scaled winit pixels.
    let (x, y) = centered_child_origin(
        (parent_frame.origin.x, parent_frame.origin.y),
        (parent_frame.size.width, parent_frame.size.height),
        (frame.size.width, frame.size.height),
    );
    frame.origin = NSPoint { x, y };
    if let Some(screen) = parent.screen() {
        frame = unsafe { child.constrainFrameRect_toScreen(frame, Some(&screen)) };
    }
    unsafe {
        child.setFrameOrigin(frame.origin);
        if let Some(previous) = child.parentWindow() {
            if std::ptr::eq(&*previous, parent) {
                return;
            }
            previous.removeChildWindow(child);
        }
        parent.addChildWindow_ordered(child, NSWindowOrderingMode::NSWindowAbove);
    }
}

pub(crate) fn detach_native_window(window: &NSWindow) {
    unsafe {
        if let Some(parent) = window.parentWindow() {
            parent.removeChildWindow(window);
        }
        if let Some(children) = window.childWindows() {
            for child in children.to_vec_retained() {
                window.removeChildWindow(&child);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn child_windows_center_over_their_parent_in_global_points() {
        assert_eq!(
            super::centered_child_origin((100.0, 50.0), (1000.0, 800.0), (400.0, 300.0)),
            (400.0, 300.0)
        );
        assert_eq!(
            super::centered_child_origin((-1600.0, 50.0), (1000.0, 800.0), (400.0, 300.0)),
            (-1300.0, 300.0)
        );
        assert_eq!(
            super::centered_child_origin((100.0, -1200.0), (1000.0, 800.0), (400.0, 300.0)),
            (400.0, -950.0)
        );
        assert_eq!(
            super::centered_child_origin((10.0, 0.0), (300.0, 200.0), (600.0, 500.0)),
            (-140.0, -150.0)
        );
    }
}
