//! Modeless diagnostics: inspection runs off the compositor thread.

use std::cell::RefCell;

use objc2_app_kit::NSTextView;

use super::*;

static RUNNING: AtomicBool = AtomicBool::new(false);

thread_local! {
    static REPORT: RefCell<Option<ReportWindow>> = const { RefCell::new(None) };
}

struct ReportWindow {
    window: Retained<NSWindow>,
    text: Retained<NSTextView>,
    retry: Retained<NSButton>,
    copy: Retained<NSButton>,
}

impl ReportWindow {
    unsafe fn new(mtm: MainThreadMarker) -> Self {
        unsafe {
            ensure_handler();
            let handler = HANDLER.lock().unwrap().unwrap() as *mut AnyObject;
            let window: Retained<NSWindow> = msg_send_id![mtm.alloc::<NSWindow>(),
                initWithContentRect: rect(0.0, 0.0, 700.0, 540.0)
                styleMask: NSWindowStyleMask::Titled | NSWindowStyleMask::Closable
                    | NSWindowStyleMask::Resizable | NSWindowStyleMask::Miniaturizable
                backing: NSBackingStoreType::NSBackingStoreBuffered defer: false];
            window.setReleasedWhenClosed(false);
            window.setMinSize(NSSize {
                width: 480.0,
                height: 320.0,
            });
            window.center();
            let root: Retained<NSView> = msg_send_id![mtm.alloc::<NSView>(),
                initWithFrame: rect(0.0, 0.0, 700.0, 540.0)];
            let retry = add_button(
                &root,
                "Run Diagnostics",
                rect(16.0, 496.0, 166.0, 30.0),
                handler,
                sel!(runStartupDiagnostics:),
                mtm,
            );
            let copy = add_button(
                &root,
                "Copy Report",
                rect(194.0, 496.0, 142.0, 30.0),
                handler,
                sel!(copyStartupDiagnostics:),
                mtm,
            );
            for button in [&retry, &copy] {
                button.setAutoresizingMask(NSAutoresizingMaskOptions::NSViewMinYMargin);
            }
            let scroll = NSScrollView::initWithFrame(
                mtm.alloc::<NSScrollView>(),
                rect(16.0, 16.0, 668.0, 468.0),
            );
            scroll.setHasVerticalScroller(true);
            scroll.setHasHorizontalScroller(false);
            scroll.setAutohidesScrollers(true);
            scroll.setAutoresizingMask(
                NSAutoresizingMaskOptions::NSViewWidthSizable
                    | NSAutoresizingMaskOptions::NSViewHeightSizable,
            );
            let width = scroll.contentSize().width;
            let text: Retained<NSTextView> = msg_send_id![mtm.alloc::<NSTextView>(),
                initWithFrame: rect(0.0, 0.0, width, 468.0)];
            text.setEditable(false);
            text.setSelectable(true);
            text.setRichText(false);
            text.setFont(Some(&label_font(TextStyle::Body)));
            text.setMinSize(NSSize {
                width: 0.0,
                height: 0.0,
            });
            text.setMaxSize(NSSize {
                width: f64::MAX,
                height: f64::MAX,
            });
            text.setHorizontallyResizable(false);
            text.setVerticallyResizable(true);
            text.setAutoresizingMask(NSAutoresizingMaskOptions::NSViewWidthSizable);
            let container: *mut AnyObject = msg_send![&*text, textContainer];
            if !container.is_null() {
                let _: () = msg_send![container, setWidthTracksTextView: true];
                let _: () =
                    msg_send![container, setContainerSize: NSSize { width, height: f64::MAX }];
            }
            scroll.setDocumentView(Some(&text));
            root.addSubview(&scroll);
            window.setContentView(Some(&root));
            Self {
                window,
                text,
                retry,
                copy,
            }
        }
    }

    fn update(&self, text: &str, running: bool) {
        unsafe {
            self.window
                .setTitle(&NSString::from_str(&tr("Startup Diagnostics")));
            self.text.setString(&NSString::from_str(text));
            self.retry
                .setTitle(&NSString::from_str(&tr("Run Diagnostics")));
            self.retry.setEnabled(!running);
            self.copy.setTitle(&NSString::from_str(&tr("Copy Report")));
            self.copy.setEnabled(!running);
        }
    }
}

pub(super) fn start(sender: Sender<CompositorMessage>, mtm: MainThreadMarker) {
    if crate::lifecycle_producers::GLOBAL.closing() {
        return;
    }
    let started = !RUNNING.swap(true, Ordering::AcqRel);
    REPORT.with(|report| {
        let mut report = report.borrow_mut();
        let window = report.get_or_insert_with(|| unsafe { ReportWindow::new(mtm) });
        if started {
            window.update(&tr("Running startup diagnostics in the background. No runtimes or containers will be started."), true);
        }
        window.window.makeKeyAndOrderFront(None);
    });
    if !started {
        return;
    }
    let Some(producer) = crate::lifecycle_producers::GLOBAL.begin() else {
        RUNNING.store(false, Ordering::Release);
        return;
    };
    if let Err(error) = std::thread::Builder::new()
        .name("startup-diagnostics".into())
        .spawn(move || {
            let _producer = producer;
            let report = crate::startup_diagnostics::collect();
            if sender
                .send(CompositorMessage::StartupDiagnosticsFinished(report))
                .is_err()
            {
                RUNNING.store(false, Ordering::Release);
            }
        })
    {
        let mut report = crate::startup_diagnostics::Report::default();
        report.push(
            "Startup Diagnostics",
            crate::startup_diagnostics::Status::Failed,
            error.to_string(),
            "The diagnostic worker could not start. Close unused processes and retry.",
        );
        finish(report);
    }
}

pub(super) fn finish(mut report: crate::startup_diagnostics::Report) {
    RUNNING.store(false, Ordering::Release);
    for (_, state) in SESSION_STATES
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, state)| state.failed_step.is_some() || state.profile == ProfileStatus::Blocked)
        .take(4)
    {
        report.push("Recent application failure", crate::startup_diagnostics::Status::Warning,
            state.detail.clone(), "This is a previous application error, not a fresh probe. Open that application's Logs and Run Health Check to confirm its current state.");
    }
    REPORT.with(|window| {
        if let Some(window) = window.borrow().as_ref() {
            // Do not steal focus or reopen a report the user has closed.
            window.update(&report.text(), false);
        }
    });
}

pub(super) fn copy() {
    REPORT.with(|window| {
        if let Some(window) = window.borrow().as_ref() {
            unsafe {
                let pasteboard = NSPasteboard::generalPasteboard();
                pasteboard.clearContents();
                pasteboard.setString_forType(&window.text.string(), NSPasteboardTypeString);
            }
        }
    });
}
