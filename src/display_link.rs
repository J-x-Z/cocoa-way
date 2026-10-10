//! Opt-in macOS 14+ CAMetalDisplayLink experiment, not a default frame scheduler.
//! The parent requests only real demand, waits for has_drawable(), then renders
//! with take_drawable() instead of calling nextDrawable(). Keep VSync enabled.
//! Callbacks supply a drawable, not proof or a timestamp of actual presentation.
//! Hidden/occluded windows may not get callbacks; the parent owns fallback policy.

use std::cell::{Cell, RefCell};
use std::ffi::OsStr;
use std::rc::Rc;
use std::sync::Arc;

use objc2::mutability::MainThreadOnly;
use objc2::rc::{Allocated, Retained};
use objc2::runtime::{AnyClass, AnyObject, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{ClassType, DeclaredClass, ProtocolType, declare_class, extern_protocol};
use objc2::{msg_send, msg_send_id};
use objc2_foundation::{MainThreadMarker, NSString};
use objc2_quartz_core::{CAMetalDrawable, CAMetalLayer};
use winit::window::Window;

type Drawable = Retained<ProtocolObject<dyn CAMetalDrawable>>;
type ReadyDrawable = Rc<RefCell<Option<Drawable>>>;

// NSRunLoop bindings are feature-disabled. This matches Foundation's public symbol.
#[link(name = "Foundation", kind = "framework")]
unsafe extern "C" {
    static NSRunLoopCommonModes: &'static NSString;
}

// CAMetalDisplayLink bindings are feature-disabled; all messages match its SDK header.
extern_protocol!(
    unsafe trait CAMetalDisplayLinkDelegate {
        #[method(metalDisplayLink:needsUpdate:)]
        unsafe fn needs_update(&self, link: &AnyObject, update: &AnyObject);
    }

    unsafe impl ProtocolType for dyn CAMetalDisplayLinkDelegate {}
);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Demand {
    Idle,
    Waiting,
    Ready,
}

fn opted_in(value: Option<&OsStr>) -> bool {
    value == Some(OsStr::new("1"))
}

fn arm_demand(demand: &Cell<Demand>) -> bool {
    if demand.get() != Demand::Idle {
        return false;
    }
    demand.set(Demand::Waiting);
    true
}

fn accept_ready<T>(demand: &Cell<Demand>, ready: &RefCell<Option<T>>, value: T) -> bool {
    if demand.get() != Demand::Waiting || ready.borrow().is_some() {
        return false;
    }
    *ready.borrow_mut() = Some(value);
    demand.set(Demand::Ready);
    true
}

fn take_ready<T>(demand: &Cell<Demand>, ready: &RefCell<Option<T>>) -> Option<T> {
    let value = ready.borrow_mut().take();
    if value.is_some() {
        demand.set(Demand::Idle);
    }
    value
}

fn clear_ready<T>(demand: &Cell<Demand>, ready: &RefCell<Option<T>>) {
    let value = ready.borrow_mut().take();
    demand.set(Demand::Idle);
    drop(value);
}

struct DelegateState {
    demand: Cell<Demand>,
    ready: ReadyDrawable,
    window: Arc<Window>,
}

declare_class!(
    struct FrameDelegate;

    unsafe impl ClassType for FrameDelegate {
        type Super = NSObject;
        type Mutability = MainThreadOnly;
        const NAME: &'static str = "CocoaWayNativeFrameDelegate";
    }

    impl DeclaredClass for FrameDelegate {
        type Ivars = DelegateState;
    }

    unsafe impl NSObjectProtocol for FrameDelegate {}

    unsafe impl CAMetalDisplayLinkDelegate for FrameDelegate {
        #[method(metalDisplayLink:needsUpdate:)]
        unsafe fn needs_update(&self, link: &AnyObject, update: &AnyObject) {
            let state = self.ivars();
            if state.demand.get() != Demand::Waiting {
                unsafe { let _: () = msg_send![link, setPaused: true]; }
                return;
            }
            // The SDK guarantees non-null. A nil violation retains pending demand,
            // allowing a subsequent native update to satisfy it without a timer.
            let drawable: Option<Drawable> = unsafe { msg_send_id![update, drawable] };
            let Some(drawable) = drawable else { return; };
            let redraw = accept_ready(&state.demand, &state.ready, drawable);
            // Release all RefCell borrows before messages or event-loop notification.
            unsafe { let _: () = msg_send![link, setPaused: true]; }
            if redraw {
                state.window.request_redraw();
            }
        }
    }
);

/// Main-thread-only source: one pending request and at most one retained drawable.
pub struct NativeFrameSource {
    link: Retained<NSObject>,
    delegate: Retained<FrameDelegate>,
    ready: ReadyDrawable,
    run_loop: Retained<NSObject>,
    mode: &'static NSString,
}

impl NativeFrameSource {
    /// None means disabled or unavailable. Construction errors leave no registration.
    /// The delegate's Rc state prevents moving the source or dropping it off-main.
    pub fn new(layer: &CAMetalLayer, window: Arc<Window>) -> Result<Option<Self>, String> {
        if !opted_in(std::env::var_os("COCOA_WAY_NATIVE_FRAME_SOURCE").as_deref()) {
            return Ok(None);
        }
        // The public class/protocol were introduced in macOS 14. Runtime lookup
        // avoids linking new class symbols into the macOS 11 deployment target.
        let Some(class) = AnyClass::get("CAMetalDisplayLink") else {
            return Ok(None);
        };
        if <dyn CAMetalDisplayLinkDelegate>::protocol().is_none() {
            return Ok(None);
        }
        let thread_class =
            AnyClass::get("NSThread").ok_or_else(|| "NSThread is unavailable".to_owned())?;
        let on_main: bool = unsafe { msg_send![thread_class, isMainThread] };
        if !on_main {
            return Err("NativeFrameSource must be created on the main thread".to_owned());
        }
        // MainThreadMarker::new is feature-disabled; NSThread just verified this.
        let mtm = unsafe { MainThreadMarker::new_unchecked() };
        let run_loop_class =
            AnyClass::get("NSRunLoop").ok_or_else(|| "NSRunLoop is unavailable".to_owned())?;
        let run_loop: Option<Retained<NSObject>> =
            unsafe { msg_send_id![run_loop_class, currentRunLoop] };
        let run_loop = run_loop.ok_or_else(|| "currentRunLoop returned nil".to_owned())?;
        let allocated: Allocated<NSObject> = unsafe { msg_send_id![class, alloc] };
        let link: Option<Retained<NSObject>> =
            unsafe { msg_send_id![allocated, initWithMetalLayer: layer] };
        let link =
            link.ok_or_else(|| "CAMetalDisplayLink initialization returned nil".to_owned())?;
        let ready = Rc::new(RefCell::new(None));
        let this = mtm.alloc::<FrameDelegate>().set_ivars(DelegateState {
            demand: Cell::new(Demand::Idle),
            ready: Rc::clone(&ready),
            window,
        });
        let delegate: Option<Retained<FrameDelegate>> = unsafe { msg_send_id![super(this), init] };
        let delegate =
            delegate.ok_or_else(|| "Frame delegate initialization returned nil".to_owned())?;
        let mode = unsafe { NSRunLoopCommonModes };
        let source = Self {
            link,
            delegate,
            ready,
            run_loop,
            mode,
        };
        let delegate: &ProtocolObject<dyn CAMetalDisplayLinkDelegate> =
            ProtocolObject::from_ref(&*source.delegate);
        unsafe {
            let _: () = msg_send![&*source.link, setPaused: true];
            let _: () = msg_send![&*source.link, setPreferredFrameLatency: 1.0_f32];
            // Leave preferredFrameRateRange at its system-selected default.
            let _: () = msg_send![&*source.link, setDelegate: delegate];
            let _: () =
                msg_send![&*source.link, addToRunLoop: &*source.run_loop forMode: source.mode];
        }
        Ok(Some(source))
    }

    /// Coalesce repeated demand; a ready drawable already satisfies a new request.
    pub fn request_frame(&self) {
        if arm_demand(&self.delegate.ivars().demand) {
            unsafe {
                let _: () = msg_send![&*self.link, setPaused: false];
            }
        }
    }

    pub fn has_drawable(&self) -> bool {
        self.ready.borrow().is_some()
    }

    /// Check readiness before consuming frame IDs or clock milestones in the parent.
    pub fn take_drawable(&self) -> Option<Drawable> {
        take_ready(&self.delegate.ivars().demand, &self.ready)
    }
}

impl Drop for NativeFrameSource {
    fn drop(&mut self) {
        unsafe {
            let _: () = msg_send![&*self.link, setPaused: true];
            let _: () = msg_send![&*self.link, setDelegate: None::<&AnyObject>];
            let _: () =
                msg_send![&*self.link, removeFromRunLoop: &*self.run_loop forMode: self.mode];
            let _: () = msg_send![&*self.link, invalidate];
        }
        clear_ready(&self.delegate.ivars().demand, &self.ready);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Token(Rc<Cell<usize>>);
    impl Drop for Token {
        fn drop(&mut self) {
            self.0.set(self.0.get() + 1);
        }
    }

    #[test]
    fn only_explicit_one_enables_the_experiment() {
        for value in [
            None,
            Some(OsStr::new("")),
            Some(OsStr::new("0")),
            Some(OsStr::new("true")),
        ] {
            assert!(!opted_in(value));
        }
        assert!(opted_in(Some(OsStr::new("1"))));
    }

    #[test]
    fn requests_coalesce_while_waiting_and_while_ready() {
        let demand = Cell::new(Demand::Idle);
        let ready = RefCell::new(None);
        assert!(arm_demand(&demand));
        assert!(!arm_demand(&demand));
        assert!(accept_ready(&demand, &ready, 1));
        assert!(!arm_demand(&demand));
        assert_eq!(take_ready(&demand, &ready), Some(1));
        assert_eq!(demand.get(), Demand::Idle);
        assert!(arm_demand(&demand));
    }

    #[test]
    fn unsolicited_and_duplicate_updates_drop_incoming_not_ready() {
        let demand = Cell::new(Demand::Idle);
        let ready = RefCell::new(None);
        let dropped = Rc::new(Cell::new(0));
        assert!(!accept_ready(&demand, &ready, Token(Rc::clone(&dropped))));
        assert_eq!(dropped.get(), 1);
        assert!(arm_demand(&demand));
        assert!(accept_ready(&demand, &ready, Token(Rc::clone(&dropped))));
        assert!(!accept_ready(&demand, &ready, Token(Rc::clone(&dropped))));
        assert_eq!(dropped.get(), 2);
        assert!(ready.borrow().is_some());
        let retained = take_ready(&demand, &ready).unwrap();
        assert_eq!(dropped.get(), 2);
        drop(retained);
        assert_eq!(dropped.get(), 3);
    }

    #[test]
    fn taking_without_readiness_preserves_waiting_demand() {
        let demand = Cell::new(Demand::Idle);
        let ready = RefCell::<Option<u8>>::new(None);
        assert_eq!(take_ready(&demand, &ready), None);
        assert!(arm_demand(&demand));
        assert_eq!(take_ready(&demand, &ready), None);
        assert_eq!(demand.get(), Demand::Waiting);
        assert!(accept_ready(&demand, &ready, 1));
    }

    #[test]
    fn teardown_releases_ready_drawable_and_pending_demand() {
        let demand = Cell::new(Demand::Idle);
        let ready = RefCell::new(None);
        let dropped = Rc::new(Cell::new(0));
        assert!(arm_demand(&demand));
        assert!(accept_ready(&demand, &ready, Token(Rc::clone(&dropped))));
        clear_ready(&demand, &ready);
        assert_eq!(demand.get(), Demand::Idle);
        assert!(ready.borrow().is_none());
        assert_eq!(dropped.get(), 1);
        assert!(arm_demand(&demand));
        clear_ready(&demand, &ready);
        assert_eq!(demand.get(), Demand::Idle);
    }
}
