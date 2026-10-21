//! Optional, once-per-baseline notice. Probe only from the primary GUI startup;
//! the parent must not invoke it in display workers, relays or command-line tools.

use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use objc2_foundation::MainThreadMarker;

use crate::event_loop_signal::Sender;
use crate::messages::CompositorMessage;
use crate::startup_diagnostics::APPLE_SECURITY_BASELINE;

type Version = (u64, u64, u64);

const PROBE_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_VERSION_OUTPUT: usize = 4096;
const PREFERENCE_SUITE: &str = "com.jxz.cocoa-way";
const DISMISSAL_VALUE: &str = "dismissed";
const OFFICIAL_RELEASE_URL: &str = "https://github.com/apple/container/releases/latest";
static PROBE_STARTED: AtomicBool = AtomicBool::new(false);

fn version_text(version: Version) -> String {
    format!("{}.{}.{}", version.0, version.1, version.2)
}

fn parse_version(text: &str) -> Option<Version> {
    if text.len() > 128 {
        return None;
    }
    let text = text.strip_prefix('v').unwrap_or(text);
    let (core, metadata) = text.split_once('+').unwrap_or((text, ""));
    if text.contains('+')
        && (metadata.is_empty()
            || metadata.split('.').any(|part| {
                part.is_empty()
                    || !part
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            }))
    {
        return None;
    }
    let mut parts = core.split('.');
    let mut number = || {
        let part = parts.next()?;
        if part.is_empty()
            || (part.len() > 1 && part.starts_with('0'))
            || !part.bytes().all(|byte| byte.is_ascii_digit())
        {
            return None;
        }
        part.parse::<u64>().ok()
    };
    let version = (number()?, number()?, number()?);
    parts.next().is_none().then_some(version)
}

fn confirmed_cli_version(success: bool, stdout: &[u8]) -> Option<Version> {
    if !success || stdout.len() > MAX_VERSION_OUTPUT {
        return None;
    }
    let output = std::str::from_utf8(stdout).ok()?.trim();
    // Do not mistake a build, dependency, warning or prerelease number for the CLI.
    if output.lines().count() != 1 {
        return None;
    }
    let version = output
        .strip_prefix("container CLI version ")
        .or_else(|| output.strip_prefix("container version "))?;
    let (version, suffix) = version
        .split_once(char::is_whitespace)
        .unwrap_or((version, ""));
    let suffix = suffix.trim();
    if !suffix.is_empty()
        && !(suffix.starts_with('(')
            && suffix.ends_with(')')
            && suffix
                .bytes()
                .all(|byte| byte.is_ascii_graphic() || byte == b' '))
    {
        return None;
    }
    parse_version(version)
}

fn dismissal_key(baseline: Version) -> String {
    format!("appleContainerUpdateDismissed.{}", version_text(baseline))
}

fn should_offer(installed: Option<Version>, baseline: Version, dismissed: bool) -> bool {
    !dismissed && installed.is_some_and(|installed| installed < baseline)
}

fn should_record_dismissal(presented: bool, acknowledged: bool) -> bool {
    presented && !acknowledged
}

/// Starts at most one background, read-only installed-CLI probe per process.
/// Missing/unknown/newer versions and unavailable preferences do not notify.
/// Runtime command capture owns timeout, bounded output and child termination.
pub fn probe(sender: Sender<CompositorMessage>) {
    if crate::lifecycle_producers::GLOBAL.closing() || PROBE_STARTED.swap(true, Ordering::AcqRel) {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("apple-update-notice".into())
        .spawn(move || {
            objc2::rc::autoreleasepool(|_| {
                if native::dismissed(APPLE_SECURITY_BASELINE) != Some(false)
                    || crate::lifecycle_producers::GLOBAL.closing()
                {
                    return;
                }
                let path = crate::runtime_paths::build_child_path();
                let Some(executable) = crate::runtime_paths::find_command_path("container", &path)
                else {
                    return;
                };
                let mut command = Command::new(executable);
                command.env("PATH", path).arg("--version");
                let Ok(output) = crate::runtime_commands::capture(command, PROBE_TIMEOUT) else {
                    return;
                };
                let installed = confirmed_cli_version(output.status.success(), &output.stdout);
                if !should_offer(installed, APPLE_SECURITY_BASELINE, false)
                    || crate::lifecycle_producers::GLOBAL.closing()
                {
                    return;
                }
                if let Some(installed) = installed {
                    // An optional notice may be skipped when the event queue is full.
                    // No preference is written, so a later launch can try again.
                    let _ = sender.try_send(CompositorMessage::AppleContainerUpdateAvailable(
                        version_text(installed),
                    ));
                }
            });
        });
    if spawned.is_err() {
        PROBE_STARTED.store(false, Ordering::Release);
        log::debug!("Could not spawn the optional Apple Container version probe");
    }
}

/// Call only on the primary GUI's main-thread event loop after startup is ready.
/// Never enters a modal loop, activates the app, or creates a compositor display.
pub fn present(installed: &str, mtm: MainThreadMarker) {
    native::present(installed, mtm);
}

mod native {
    use std::cell::{Cell, RefCell};

    use objc2::mutability::MainThreadOnly;
    use objc2::rc::Retained;
    use objc2::runtime::{AnyClass, AnyObject, NSObject};
    use objc2::{ClassType, DeclaredClass, declare_class, msg_send, msg_send_id, sel};
    use objc2_app_kit::{
        NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSButton, NSFont,
        NSLayoutAttribute as Attribute, NSLayoutConstraint, NSLayoutRelation, NSPanel, NSStackView,
        NSStackViewDistribution, NSTextField, NSUserInterfaceLayoutOrientation, NSView,
        NSWindowStyleMask,
    };
    use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};

    use super::*;
    use crate::i18n::{tr, tr_format};

    // The crate's NSApp() wrapper calls sharedApplication and creates one if absent.
    // Read the SDK global instead: notices must not initialize GUI worker instances.
    #[link(name = "AppKit", kind = "framework")]
    unsafe extern "C" {
        static NSApp: *mut NSApplication;
    }

    fn defaults() -> Option<Retained<AnyObject>> {
        let class = AnyClass::get("NSUserDefaults")?;
        unsafe {
            let allocated = msg_send_id![class, alloc];
            msg_send_id![allocated, initWithSuiteName: &*NSString::from_str(PREFERENCE_SUITE)]
        }
    }

    pub(super) fn dismissed(baseline: Version) -> Option<bool> {
        let defaults = defaults()?;
        unsafe {
            let value: Option<Retained<AnyObject>> = msg_send_id![&*defaults,
                objectForKey: &*NSString::from_str(&dismissal_key(baseline))];
            let Some(value) = value else {
                return Some(false);
            };
            let is_string: bool = msg_send![&*value, isKindOfClass: NSString::class()];
            if !is_string {
                return Some(false);
            }
            let value: Retained<NSString> = Retained::cast(value);
            Some(value.to_string() == DISMISSAL_VALUE)
        }
    }

    fn persist_dismissal(baseline: Version) {
        let Some(defaults) = defaults() else {
            log::warn!("Apple Container notice dismissal preferences are unavailable");
            return;
        };
        unsafe {
            let _: () = msg_send![&*defaults, setObject: &*NSString::from_str(DISMISSAL_VALUE)
                forKey: &*NSString::from_str(&dismissal_key(baseline))];
        }
        // NSUserDefaults persists asynchronously; do not synchronize on AppKit.
    }

    struct HandlerState {
        baseline: Version,
        presented: Cell<bool>,
        acknowledged: Cell<bool>,
    }

    declare_class!(
        struct NoticeHandler;

        unsafe impl ClassType for NoticeHandler {
            type Super = NSObject;
            type Mutability = MainThreadOnly;
            const NAME: &'static str = "CocoaWayAppleContainerUpdateNoticeHandler";
        }

        impl DeclaredClass for NoticeHandler {
            type Ivars = HandlerState;
        }

        unsafe impl NoticeHandler {
            #[method(windowShouldClose:)]
            fn window_should_close(&self, _window: &AnyObject) -> bool {
                self.acknowledge();
                true
            }

            #[method(dismissAppleContainerUpdate:)]
            fn dismiss(&self, sender: &AnyObject) {
                self.acknowledge();
                close_button_window(sender);
            }

            #[method(openAppleContainerRelease:)]
            fn open_release(&self, sender: &AnyObject) {
                let acknowledged = self.acknowledge();
                close_button_window(sender);
                if acknowledged {
                    open_official_release();
                }
            }
        }
    );

    impl NoticeHandler {
        fn acknowledge(&self) -> bool {
            let state = self.ivars();
            if !should_record_dismissal(state.presented.get(), state.acknowledged.get()) {
                return false;
            }
            state.acknowledged.set(true);
            persist_dismissal(state.baseline);
            true
        }
    }

    fn close_button_window(sender: &AnyObject) {
        unsafe {
            let window: Option<Retained<NSPanel>> = msg_send_id![sender, window];
            if let Some(window) = window {
                let _: () = msg_send![&*window, close];
            }
        }
    }

    fn open_official_release() {
        // Only the explicit button action reaches this code; never use CLI output as a URL.
        let (Some(url_class), Some(workspace_class)) =
            (AnyClass::get("NSURL"), AnyClass::get("NSWorkspace"))
        else {
            return;
        };
        unsafe {
            let url: Option<Retained<AnyObject>> = msg_send_id![url_class,
                URLWithString: &*NSString::from_str(OFFICIAL_RELEASE_URL)];
            let workspace: Option<Retained<AnyObject>> =
                msg_send_id![workspace_class, sharedWorkspace];
            if let (Some(url), Some(workspace)) = (url, workspace) {
                let opened: bool = msg_send![&*workspace, openURL: &*url];
                if !opened {
                    log::warn!("Could not open the official Apple Container release page");
                }
            }
        }
    }

    struct Notice {
        _panel: Retained<NSPanel>,
        // AppKit's delegate and control targets are non-owning.
        _handler: Retained<NoticeHandler>,
    }

    thread_local! {
        // Keep one closed panel/handler alive until exit, never drop a callback's receiver.
        static NOTICE: RefCell<Option<Notice>> = const { RefCell::new(None) };
    }

    fn rect(width: f64, height: f64) -> NSRect {
        NSRect {
            origin: NSPoint { x: 0.0, y: 0.0 },
            size: NSSize { width, height },
        }
    }

    fn constrain(view: &NSView, attribute: Attribute, other: Option<(&NSView, Attribute)>, n: f64) {
        unsafe {
            let constraint = NSLayoutConstraint::constraintWithItem_attribute_relatedBy_toItem_attribute_multiplier_constant(
                view, attribute, NSLayoutRelation::Equal,
                other.map(|(view, _)| view as &AnyObject),
                other.map_or(Attribute::NotAnAttribute, |(_, attribute)| attribute),
                1.0, n,
            );
            constraint.setActive(true);
        }
    }

    fn stack(vertical: bool, mtm: MainThreadMarker) -> Retained<NSStackView> {
        unsafe {
            let stack: Retained<NSStackView> =
                msg_send_id![mtm.alloc::<NSStackView>(), initWithFrame: rect(0.0, 0.0)];
            stack.setTranslatesAutoresizingMaskIntoConstraints(false);
            stack.setOrientation(if vertical {
                NSUserInterfaceLayoutOrientation::Vertical
            } else {
                NSUserInterfaceLayoutOrientation::Horizontal
            });
            stack.setAlignment(if vertical {
                Attribute::Leading
            } else {
                Attribute::CenterY
            });
            stack.setDistribution(NSStackViewDistribution::Fill);
            stack.setSpacing(14.0);
            stack
        }
    }

    unsafe fn build(installed: Version, mtm: MainThreadMarker) -> Option<Notice> {
        unsafe {
            let panel: Option<Retained<NSPanel>> = msg_send_id![mtm.alloc::<NSPanel>(),
                initWithContentRect: rect(520.0, 240.0)
                styleMask: NSWindowStyleMask::Titled | NSWindowStyleMask::Closable
                    | NSWindowStyleMask::NonactivatingPanel
                backing: NSBackingStoreType::NSBackingStoreBuffered defer: false];
            let panel = panel?;
            panel.setReleasedWhenClosed(false);
            panel.setFloatingPanel(false);
            panel.setLevel(0);
            panel.setHidesOnDeactivate(false);
            panel.setBecomesKeyOnlyIfNeeded(true);
            panel.setExcludedFromWindowsMenu(true);
            panel.setTitle(&NSString::from_str(&tr("Apple Container Update")));
            let allocated = mtm.alloc::<NoticeHandler>().set_ivars(HandlerState {
                baseline: APPLE_SECURITY_BASELINE,
                presented: Cell::new(false),
                acknowledged: Cell::new(false),
            });
            let handler: Option<Retained<NoticeHandler>> = msg_send_id![super(allocated), init];
            let handler = handler?;
            let _: () = msg_send![&*panel, setDelegate: &*handler];
            let root: Retained<NSView> =
                msg_send_id![mtm.alloc::<NSView>(), initWithFrame: rect(520.0, 240.0)];
            let column = stack(true, mtm);
            root.addSubview(&column);
            constrain(
                &column,
                Attribute::Leading,
                Some((&root, Attribute::Leading)),
                20.0,
            );
            constrain(
                &column,
                Attribute::Top,
                Some((&root, Attribute::Top)),
                -20.0,
            );
            constrain(&column, Attribute::Width, None, 480.0);
            let heading = tr_format(
                "Apple Container {installed} is older than Cocoa-Way's security baseline {baseline}.",
                &[
                    ("installed", &version_text(installed)),
                    ("baseline", &version_text(APPLE_SECURITY_BASELINE)),
                ],
            );
            let explanation = tr(
                "Apple Container is optional. You can keep using Docker, OrbStack and SSH. This reminder is shown once per security baseline; nothing is installed or started automatically.",
            );
            for (text, bold) in [(heading.as_str(), true), (explanation.as_ref(), false)] {
                let label = NSTextField::wrappingLabelWithString(&NSString::from_str(text), mtm);
                label.setTranslatesAutoresizingMaskIntoConstraints(false);
                let font = if bold {
                    NSFont::boldSystemFontOfSize(14.0)
                } else {
                    NSFont::systemFontOfSize(13.0)
                };
                let _: () = msg_send![&*label, setFont: &*font];
                let _: () = msg_send![&*label, setMaximumNumberOfLines: 0isize];
                let _: () = msg_send![&*label, setPreferredMaxLayoutWidth: 480.0f64];
                column.addArrangedSubview(&label);
                constrain(&label, Attribute::Width, None, 480.0);
            }
            let buttons = stack(false, mtm);
            for (title, action) in [
                ("Dismiss", sel!(dismissAppleContainerUpdate:)),
                ("Open Official Release", sel!(openAppleContainerRelease:)),
            ] {
                let button = NSButton::buttonWithTitle_target_action(
                    &NSString::from_str(&tr(title)),
                    Some(&*handler),
                    Some(action),
                    mtm,
                );
                button.setTranslatesAutoresizingMaskIntoConstraints(false);
                buttons.addArrangedSubview(&button);
            }
            column.addArrangedSubview(&buttons);
            panel.setContentView(Some(&root));
            let fitting: NSSize = msg_send![&*column, fittingSize];
            if !fitting.height.is_finite() || !(0.0..=600.0).contains(&fitting.height) {
                let _: () = msg_send![&*panel, setDelegate: None::<&AnyObject>];
                let _: () = msg_send![&*panel, close];
                return None;
            }
            panel.setContentSize(NSSize {
                width: 520.0,
                height: (fitting.height + 40.0).max(180.0),
            });
            panel.center();
            // Nonactivating, modeless ordering only: never makeKey, activate or runModal.
            panel.orderFront(None);
            if !panel.isVisible() {
                let _: () = msg_send![&*panel, setDelegate: None::<&AnyObject>];
                let _: () = msg_send![&*panel, close];
                return None;
            }
            handler.ivars().presented.set(true);
            Some(Notice {
                _panel: panel,
                _handler: handler,
            })
        }
    }

    pub(super) fn present(installed: &str, mtm: MainThreadMarker) {
        if crate::lifecycle_producers::GLOBAL.closing() {
            return;
        }
        let Some(dismissed) = dismissed(APPLE_SECURITY_BASELINE) else {
            return;
        };
        let installed = parse_version(installed);
        if !should_offer(installed, APPLE_SECURITY_BASELINE, dismissed) {
            return;
        }
        // Do not create NSApplication, unhide it, change its activation policy or
        // show a notice in headless/prohibited instances. The primary GUI owns admission.
        let Some(app) = (unsafe { NSApp.as_ref() }) else {
            return;
        };
        if unsafe { app.activationPolicy() } == NSApplicationActivationPolicy::Prohibited
            || unsafe { app.isHidden() }
        {
            return;
        }
        let Some(screen_class) = AnyClass::get("NSScreen") else {
            return;
        };
        let screen: Option<Retained<AnyObject>> = unsafe { msg_send_id![screen_class, mainScreen] };
        if screen.is_none() {
            return;
        }
        NOTICE.with(|notice| {
            if notice.borrow().is_some() {
                return;
            }
            if let Some(installed) = installed
                && let Some(created) = unsafe { build(installed, mtm) }
            {
                *notice.borrow_mut() = Some(created);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASELINE: Version = (1, 6, 0);

    #[test]
    fn confirmed_older_cli_is_offered() {
        let installed = confirmed_cli_version(true, b"container CLI version 1.5.2 (build: abc)\n");
        assert_eq!(installed, Some((1, 5, 2)));
        assert!(should_offer(installed, BASELINE, false));
    }

    #[test]
    fn equal_and_newer_versions_do_not_prompt() {
        for text in ["1.6.0", "1.6.1", "1.10.0", "2.0.0", "1.6.0+build.12"] {
            assert!(!should_offer(parse_version(text), BASELINE, false));
        }
    }

    #[test]
    fn missing_failed_unknown_and_ambiguous_output_do_not_prompt() {
        for output in [
            "",
            "1.5.0",
            "unavailable",
            "build version 1.0.0",
            "container CLI version unknown (dependency 1.0.0)",
            "container CLI version 1.5.0\ncontainer CLI version 1.7.0",
            "container CLI version 1.5.0 warning: not the installed CLI",
        ] {
            assert!(!should_offer(
                confirmed_cli_version(true, output.as_bytes()),
                BASELINE,
                false
            ));
        }
        assert_eq!(
            confirmed_cli_version(false, b"container CLI version 1.5.0"),
            None
        );
        assert_eq!(confirmed_cli_version(true, &[0xff]), None);
        assert!(!should_offer(None, BASELINE, false));
    }

    #[test]
    fn malformed_prerelease_overflow_and_excess_output_are_unknown() {
        for text in [
            "1.5",
            "1.5.0.0",
            "01.5.0",
            "1.5.0-rc.1",
            "1.5.0+",
            "1.5.0+a..b",
            "1.5.0+build+other",
            "18446744073709551616.0.0",
            "-1.5.0",
            "1.5.0\0",
        ] {
            assert_eq!(parse_version(text), None, "{text}");
        }
        assert_eq!(
            confirmed_cli_version(true, &vec![b'0'; MAX_VERSION_OUTPUT + 1]),
            None
        );
    }

    #[test]
    fn supported_cli_spelling_is_normalized_without_metadata() {
        let version = confirmed_cli_version(true, b"container version v1.5.0+build.123").unwrap();
        assert_eq!(version_text(version), "1.5.0");
    }

    #[test]
    fn dismissal_suppresses_every_older_version_for_that_baseline() {
        for version in [(0, 9, 0), (1, 4, 0), (1, 5, 9)] {
            assert!(!should_offer(Some(version), BASELINE, true));
        }
    }

    #[test]
    fn dismissal_keys_are_stable_per_baseline_without_erasing_previous_ones() {
        let mut preferences = std::collections::BTreeMap::new();
        let key = dismissal_key(BASELINE);
        assert_eq!(PREFERENCE_SUITE, "com.jxz.cocoa-way");
        assert_eq!(key, "appleContainerUpdateDismissed.1.6.0");
        preferences.insert(key.clone(), DISMISSAL_VALUE);
        assert_eq!(
            preferences.get(&dismissal_key(BASELINE)),
            Some(&DISMISSAL_VALUE)
        );
        assert!(!should_offer(
            Some((1, 5, 0)),
            BASELINE,
            preferences.contains_key(&key)
        ));
        let next = (1, 7, 0);
        assert!(should_offer(
            Some((1, 6, 0)),
            next,
            preferences.contains_key(&dismissal_key(next))
        ));
        preferences.insert(dismissal_key(next), DISMISSAL_VALUE);
        assert!(preferences.contains_key(&key));
    }

    #[test]
    fn no_dismissal_is_recorded_before_successful_presentation_or_twice() {
        assert!(!should_record_dismissal(false, false));
        assert!(!should_record_dismissal(false, true));
        assert!(should_record_dismissal(true, false));
        assert!(!should_record_dismissal(true, true));
    }

    #[test]
    fn release_destination_is_fixed_official_https_not_cli_supplied() {
        assert_eq!(
            OFFICIAL_RELEASE_URL,
            "https://github.com/apple/container/releases/latest"
        );
    }
}
