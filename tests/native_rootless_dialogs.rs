#![allow(dead_code)]
#[path = "../src/rootless_native.rs"]
mod native;

use objc2::rc::Retained;
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSWindow, NSWindowStyleMask,
};
use objc2_foundation::{MainThreadMarker, NSPoint, NSRect, NSSize};

fn window(mtm: MainThreadMarker, width: f64, height: f64) -> Retained<NSWindow> {
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            mtm.alloc::<NSWindow>(),
            NSRect {
                origin: NSPoint { x: 100.0, y: 100.0 },
                size: NSSize { width, height },
            },
            NSWindowStyleMask::Titled | NSWindowStyleMask::Resizable,
            NSBackingStoreType::NSBackingStoreBuffered,
            false,
        )
    };
    unsafe {
        window.setReleasedWhenClosed(false);
    }
    window
}

fn children(window: &NSWindow) -> usize {
    unsafe { window.childWindows().map_or(0, |children| children.len()) }
}

fn main() {
    let mtm = MainThreadMarker::new().expect("AppKit main thread");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Prohibited);
    let parent = window(mtm, 900.0, 600.0);
    let replacement = window(mtm, 800.0, 500.0);
    let child = window(mtm, 424.0, 358.0);
    let grandchild = window(mtm, 180.0, 120.0);
    native::attach_native_child(&parent, &child);
    native::attach_native_child(&parent, &child);
    assert_eq!(children(&parent), 1, "idempotent attachment");
    unsafe {
        assert!(std::ptr::eq(&*child.parentWindow().unwrap(), &*parent));
    }
    let child_frame = child.frame();
    let parent_frame = parent.frame();
    assert!(
        (child_frame.origin.x
            - parent_frame.origin.x
            - (parent_frame.size.width - child_frame.size.width) / 2.0)
            .abs()
            < 1.0
    );
    assert!(
        (child_frame.origin.y
            - parent_frame.origin.y
            - (parent_frame.size.height - child_frame.size.height) / 2.0)
            .abs()
            < 1.0
    );
    native::attach_native_child(&replacement, &child);
    assert_eq!(children(&parent), 0, "reparent removes old native relation");
    assert_eq!(children(&replacement), 1);
    native::detach_native_window(&child);
    unsafe {
        assert!(child.parentWindow().is_none());
    }
    assert_eq!(children(&replacement), 0);
    native::attach_native_child(&parent, &child);
    native::attach_native_child(&child, &grandchild);
    native::detach_native_window(&parent);
    assert_eq!(children(&parent), 0);
    unsafe {
        assert!(child.parentWindow().is_none());
    }
    assert_eq!(
        children(&child),
        1,
        "retiring a parent preserves live descendant relations"
    );
    native::detach_native_window(&child);
    unsafe {
        assert!(grandchild.parentWindow().is_none());
    }
    assert_eq!(children(&child), 0);
    native::detach_native_window(&child);
    native::attach_native_child(&parent, &parent);
    assert_eq!(children(&parent), 0, "self-parent is ignored");
    for window in [&grandchild, &child, &replacement, &parent] {
        assert!(!window.isVisible(), "test must not display windows");
        window.close();
    }
    println!(
        "Native rootless dialog placement, idempotence, reparent and parent/child teardown passed; no windows displayed"
    );
}
