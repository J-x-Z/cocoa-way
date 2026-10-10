//! Opt-in interactive AppKit user-session window, not a wallpaper snapshot or
//! lock-screen provider. Reuses Winit's window and the existing Wayland seat.
//! The parent must restore before closing and restore/hide on Event::Suspended;
//! this module installs no session observer, input monitor, helper, or timer.

use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject};
use objc2::{ClassType, msg_send, msg_send_id};
use objc2_app_kit::{
    NSAlert, NSAlertSecondButtonReturn, NSView, NSWindow, NSWindowCollectionBehavior,
    NSWindowStyleMask,
};
use objc2_foundation::{MainThreadMarker, NSRect, NSString};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::{Window, WindowId};

// Public CoreGraphics declarations from CGWindowLevel.h; no private framework.
#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGWindowLevelForKey(key: i32) -> i32;
}
const DESKTOP_LEVEL_KEY: i32 = 2;
const NORMAL_LEVEL_KEY: i32 = 4;
const DESKTOP_ICON_LEVEL_KEY: i32 = 18;

struct SavedWindow {
    id: WindowId,
    native: Retained<NSWindow>,
    frame: NSRect,
    style: NSWindowStyleMask,
    behavior: NSWindowCollectionBehavior,
    level: isize,
}

impl SavedWindow {
    fn restore(&self, _mtm: MainThreadMarker) {
        self.native.setStyleMask(self.style);
        unsafe { self.native.setCollectionBehavior(self.behavior) };
        self.native.setFrame_display(self.frame, true);
        self.native.setLevel(self.level);
    }
}

/// Off by default, with confirmation scoped to this controller's lifetime.
/// Retaining the main-thread-only NSWindow also prevents moving active state
/// across threads. Drop is a fallback, not a substitute for pre-close cleanup.
#[derive(Default)]
pub struct DesktopBackground {
    original: Option<SavedWindow>,
    confirmed: bool,
}

impl DesktopBackground {
    pub fn is_active(&self) -> bool {
        self.original.is_some()
    }

    /// Returns the actual resulting state; cancellation leaves the mode off.
    pub fn toggle(&mut self, window: &Window) -> Result<bool, String> {
        let mtm = main_thread()?;
        if self.is_active() {
            self.restore(window)?;
            return Ok(false);
        }
        // Reject unsupported windows before presenting any confirmation.
        let _ = prepare_entry(window, mtm)?;
        if !self.confirmed {
            let accepted = unsafe {
                let alert = NSAlert::new(mtm);
                alert.setMessageText(&NSString::from_str(
                    "Use Desktop Background (Experimental)?",
                ));
                alert.setInformativeText(&NSString::from_str(
                    "This interactive window sits above Finder desktop icons and can block \
                     clicking or dragging them. Ordinary app windows remain above it. \
                     Click the background to focus it normally. Restore with View > Restore \
                     Desktop Window or Command+Option+B while Cocoa-Way is active.\n\n\
                     This is only an AppKit window in your user session, not a wallpaper \
                     snapshot or lock-screen provider. It uses no global input hooks and \
                     does not join all Spaces or full-screen Spaces.",
                ));
                alert.addButtonWithTitle(&NSString::from_str("Cancel"));
                alert.addButtonWithTitle(&NSString::from_str("Use Desktop Background"));
                alert.runModal() == NSAlertSecondButtonReturn
            };
            if !accepted {
                return Ok(false);
            }
            self.confirmed = true;
        }
        // The modal loop can outlive a screen/window change. Snapshot afresh.
        let (saved, screen_frame, level) = prepare_entry(window, mtm)?;
        self.original = Some(saved);
        let native = &self.original.as_ref().unwrap().native;
        // Lower before expanding, so no screen-sized ordinary window flashes.
        native.setLevel(level);
        unsafe { native.setCollectionBehavior(background_behavior()) };
        native.setStyleMask(NSWindowStyleMask::Borderless);
        native.setFrame_display(screen_frame, true);
        // No makeKey/orderFront/activation: Winit's canBecomeKeyWindow override
        // and ordinary click-to-focus continue delivering the existing events.
        Ok(true)
    }

    /// Idempotent; call before hiding/dropping the owning Winit window.
    /// A failed validation keeps the saved state available for a later retry.
    pub fn restore(&mut self, window: &Window) -> Result<(), String> {
        let mtm = main_thread()?;
        let Some(saved) = self.original.as_ref() else {
            return Ok(());
        };
        let native = native_window(window, mtm)?;
        if window.id() != saved.id || !std::ptr::eq(&*native, &*saved.native) {
            return Err("Desktop background belongs to a different window".into());
        }
        saved.restore(mtm);
        self.original = None;
        Ok(())
    }
}

impl Drop for DesktopBackground {
    fn drop(&mut self) {
        if let Some(saved) = self.original.as_ref() {
            if let Some(mtm) = MainThreadMarker::new() {
                saved.restore(mtm);
            } else {
                log::error!("Desktop background cleanup requires the AppKit main thread");
            }
        }
    }
}

fn main_thread() -> Result<MainThreadMarker, String> {
    MainThreadMarker::new()
        .ok_or_else(|| "Desktop background requires the AppKit main thread".into())
}

fn native_window(window: &Window, _mtm: MainThreadMarker) -> Result<Retained<NSWindow>, String> {
    let handle = window.window_handle().map_err(|error| error.to_string())?;
    let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
        return Err("Desktop background requires an AppKit window".into());
    };
    // Winit guarantees the borrowed handle's non-null view is live. Validate
    // Objective-C classes before casting either public API object.
    let view = unsafe { &*handle.ns_view.as_ptr().cast::<AnyObject>() };
    let is_view: bool = unsafe { msg_send![view, isKindOfClass: NSView::class()] };
    if !is_view {
        return Err("AppKit handle does not contain an NSView".into());
    }
    let native: Option<Retained<AnyObject>> = unsafe { msg_send_id![view, window] };
    let native = native.ok_or("AppKit view is not attached to a window")?;
    let is_window: bool = unsafe { msg_send![&*native, isKindOfClass: NSWindow::class()] };
    if !is_window {
        return Err("AppKit view does not belong to an NSWindow".into());
    }
    Ok(unsafe { Retained::cast(native) })
}

fn prepare_entry(
    window: &Window,
    mtm: MainThreadMarker,
) -> Result<(SavedWindow, NSRect, isize), String> {
    let native = native_window(window, mtm)?;
    let style = native.styleMask();
    if window.fullscreen().is_some() || style.contains(NSWindowStyleMask::FullScreen) {
        return Err("Exit full-screen mode before using the desktop background".into());
    }
    if native.isMiniaturized() || window.is_minimized() == Some(true) {
        return Err("Restore the minimized window before using the desktop background".into());
    }
    // NSScreen is not enabled in the existing dependency features. Use its
    // public selectors with a checked runtime class, without changing Cargo.
    let screen: Option<Retained<AnyObject>> = unsafe { msg_send_id![&*native, screen] };
    let screen = screen.ok_or("AppKit window has no native monitor")?;
    let screen_class = AnyClass::get("NSScreen").ok_or("NSScreen class is unavailable")?;
    let is_screen: bool = unsafe { msg_send![&*screen, isKindOfClass: screen_class] };
    if !is_screen {
        return Err("AppKit monitor is not an NSScreen".into());
    }
    let screen_frame: NSRect = unsafe { msg_send![&*screen, frame] };
    let scale: f64 = unsafe { msg_send![&*screen, backingScaleFactor] };
    let frame = native.frame();
    if !valid_frame(screen_frame) || !valid_frame(frame) || !scale.is_finite() || scale <= 0.0 {
        return Err("Window or native monitor has zero or invalid geometry".into());
    }
    let level = unsafe {
        background_level(
            CGWindowLevelForKey(DESKTOP_LEVEL_KEY),
            CGWindowLevelForKey(DESKTOP_ICON_LEVEL_KEY),
            CGWindowLevelForKey(NORMAL_LEVEL_KEY),
        )?
    };
    let saved = SavedWindow {
        id: window.id(),
        frame,
        style,
        behavior: unsafe { native.collectionBehavior() },
        level: unsafe { native.level() },
        native,
    };
    // NSScreen.frame is already in global logical points, including negative
    // origins on secondary monitors. Never multiply by backingScaleFactor.
    Ok((saved, screen_frame, level))
}

fn valid_frame(frame: NSRect) -> bool {
    let NSRect { origin, size } = frame;
    [origin.x, origin.y, size.width, size.height]
        .into_iter()
        .all(f64::is_finite)
        && size.width > 0.0
        && size.height > 0.0
        && (origin.x + size.width).is_finite()
        && (origin.y + size.height).is_finite()
}

fn background_level(desktop: i32, icons: i32, normal: i32) -> Result<isize, String> {
    let level = icons
        .checked_add(1)
        .filter(|level| desktop < icons && *level < normal);
    level.map(|level| level as isize).ok_or_else(|| {
        "No safe window level exists just above desktop icons and below ordinary windows".into()
    })
}

fn background_behavior() -> NSWindowCollectionBehavior {
    NSWindowCollectionBehavior::Stationary
        | NSWindowCollectionBehavior::IgnoresCycle
        | NSWindowCollectionBehavior::FullScreenNone
}

#[cfg(test)]
mod tests {
    use super::*;
    use objc2_foundation::{NSPoint, NSSize};

    fn frame(x: f64, y: f64, width: f64, height: f64) -> NSRect {
        NSRect {
            origin: NSPoint { x, y },
            size: NSSize { width, height },
        }
    }

    #[test]
    fn defaults_to_manual_off() {
        let mode = DesktopBackground::default();
        assert!(!mode.is_active());
        assert!(!mode.confirmed);
    }

    #[test]
    fn accepts_logical_secondary_monitor_geometry() {
        assert!(valid_frame(frame(-1512.0, -982.0, 1512.0, 982.0)));
        assert!(valid_frame(frame(0.5, 0.5, 1000.25, 700.25)));
    }

    #[test]
    fn rejects_empty_nonfinite_and_overflowing_frames() {
        for invalid in [
            frame(0.0, 0.0, 0.0, 100.0),
            frame(0.0, 0.0, 100.0, -1.0),
            frame(f64::NAN, 0.0, 100.0, 100.0),
            frame(0.0, f64::INFINITY, 100.0, 100.0),
            frame(0.0, 0.0, f64::INFINITY, 100.0),
            frame(f64::MAX, 0.0, f64::MAX, 100.0),
        ] {
            assert!(!valid_frame(invalid));
        }
    }

    #[test]
    fn level_is_exactly_above_icons_and_below_normal_windows() {
        assert_eq!(background_level(-100, -80, 0).unwrap(), -79);
        assert!(background_level(-100, -1, 0).is_err());
        assert!(background_level(-80, -100, 0).is_err());
        assert!(background_level(0, i32::MAX, i32::MAX).is_err());
    }

    #[test]
    fn stays_out_of_other_spaces_and_fullscreen_spaces() {
        let behavior = background_behavior();
        assert!(behavior.contains(NSWindowCollectionBehavior::FullScreenNone));
        assert!(behavior.contains(NSWindowCollectionBehavior::IgnoresCycle));
        assert!(!behavior.intersects(
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::MoveToActiveSpace
                | NSWindowCollectionBehavior::FullScreenAuxiliary
                | NSWindowCollectionBehavior::CanJoinAllApplications
                | NSWindowCollectionBehavior::Auxiliary,
        ));
    }
}
