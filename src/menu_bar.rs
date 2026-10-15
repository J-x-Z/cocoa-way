//! macOS native app-menu integration.
//!
//! Installs:  [Cocoa-Way]  [Connections ▾]
//! into NSApplication's main menu.
//!
//! "Connections" menu:
//!   - "Connect to Machine…"  → shows a quick-connect dialog
//!   - separator
//!   - saved connections from ~/.config/cocoa-way/connections.toml

use std::{cell::RefCell, sync::Mutex};

use objc2::declare_class;
use objc2::mutability::MainThreadOnly;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject};
use objc2::{ClassType, DeclaredClass, msg_send, msg_send_id, sel};
use objc2_app_kit::{
    NSAlert, NSApplication, NSEventModifierFlags, NSImage, NSMenu, NSMenuItem, NSStatusBar,
    NSStatusItem, NSVariableStatusItemLength,
};
use objc2_foundation::{MainThreadMarker, NSData, NSString};

use crate::connections::Connection;
use crate::container_sessions::ContainerSession;
use crate::event_loop_signal::Sender;
use crate::messages::CompositorMessage;

// ── Global channel sender ─────────────────────────────────────────────────────
static SENDER: Mutex<Option<Sender<CompositorMessage>>> = Mutex::new(None);

#[derive(Default)]
struct DesktopBackgroundMenuState {
    active: bool,
    items: Vec<Retained<NSMenuItem>>,
}

#[derive(Default)]
struct NativeMenuState {
    handler: Option<Retained<MenuHandler>>,
    status_item: Option<Retained<NSStatusItem>>,
}

thread_local! {
    static DESKTOP_BACKGROUND_MENU: RefCell<DesktopBackgroundMenuState> =
        RefCell::new(DesktopBackgroundMenuState::default());
    static NATIVE_MENU: RefCell<NativeMenuState> = RefCell::new(NativeMenuState::default());
}

/// Sync only after the event loop knows the actual mode state, including a
/// cancelled/failed toggle or pre-close restore. Preserved across menu reloads.
pub fn set_desktop_background_active(active: bool) {
    if MainThreadMarker::new().is_none() {
        log::error!("Desktop background menu state requires the AppKit main thread");
        return;
    }
    DESKTOP_BACKGROUND_MENU.with(|state| {
        let mut state = state.borrow_mut();
        state.active = active;
        for item in &state.items {
            unsafe {
                let _: () = msg_send![&**item, setState: isize::from(active)];
            }
        }
    });
}

fn send(msg: CompositorMessage) {
    if let Ok(g) = SENDER.lock() {
        if let Some(tx) = g.as_ref() {
            let _ = tx.send(msg);
        }
    }
}

fn sender() -> Option<Sender<CompositorMessage>> {
    SENDER.lock().ok().and_then(|g| g.as_ref().cloned())
}

// ── ObjC handler class ────────────────────────────────────────────────────────
declare_class!(
    pub struct MenuHandler;

    unsafe impl ClassType for MenuHandler {
        type Super = NSObject;
        type Mutability = MainThreadOnly;
        const NAME: &'static str = "CocoaWayMenuHandler";
    }

    impl DeclaredClass for MenuHandler {
        type Ivars = ();
    }

    unsafe impl MenuHandler {
        /// "Connect to Machine…" — shows a quick-connect NSAlert dialog.
        #[method(quickConnect:)]
        fn quick_connect(&self, _sender: &AnyObject) {
            activate_application();
            unsafe { show_quick_connect_dialog(); }
        }

        /// Connects to a saved machine. The NSMenuItem tag = index into connections list.
        #[method(connectMachine:)]
        fn connect_machine(&self, sender: &AnyObject) {
            let tag: isize = unsafe { msg_send![sender, tag] };
            send(CompositorMessage::Connect(tag as usize));
        }

        #[method(disconnectClassicConnections:)]
        fn disconnect_classic_connections(&self, _sender: &AnyObject) {
            send(CompositorMessage::DisconnectClassicConnections);
        }

        #[method(openContainerMode:)]
        fn open_container_mode(&self, _sender: &AnyObject) {
            let Some(tx) = sender() else {
                return;
            };
            // Menu actions are delivered on the main thread.
            let mtm = unsafe { MainThreadMarker::new_unchecked() };
            activate_application();
            crate::container_mode::show(tx, mtm);
        }

        #[method(openApplications:)]
        fn open_applications(&self, _sender: &AnyObject) {
            open_panel(crate::container_mode::PanelPage::Applications);
        }

        #[method(openDisplays:)]
        fn open_displays(&self, _sender: &AnyObject) {
            open_panel(crate::container_mode::PanelPage::Displays);
        }

        #[method(openDiagnostics:)]
        fn open_diagnostics(&self, _sender: &AnyObject) {
            open_panel(crate::container_mode::PanelPage::Diagnostics);
        }

        #[method(checkEnvironment:)]
        fn check_environment(&self, _sender: &AnyObject) {
            activate_application();
            let mtm = unsafe { MainThreadMarker::new_unchecked() };
            unsafe {
                let alert = NSAlert::new(mtm);
                alert.setMessageText(&NSString::from_str("Cocoa-Way Environment"));
                alert.setInformativeText(&NSString::from_str(&crate::startup::environment_report()));
                alert.addButtonWithTitle(&NSString::from_str("OK"));
                alert.addButtonWithTitle(&NSString::from_str("Open Diagnostic Logs"));
                if alert.runModal() == 1001 {
                    open_diagnostic_logs();
                }
            }
        }

        #[method(openDiagnosticLogs:)]
        fn open_diagnostic_logs(&self, _sender: &AnyObject) {
            open_diagnostic_logs();
        }

        #[method(hideDefaultDisplay:)]
        fn hide_default_display(&self, _sender: &AnyObject) {
            send(CompositorMessage::HideDefaultDisplay);
        }

        #[method(showDefaultDisplay:)]
        fn show_default_display(&self, _sender: &AnyObject) {
            send(CompositorMessage::ShowDefaultDisplay);
        }

        #[method(toggleDesktopBackground:)]
        fn toggle_desktop_background(&self, _sender: &AnyObject) {
            send(CompositorMessage::ToggleDesktopBackground);
        }

        #[method(restoreDesktopWindow:)]
        fn restore_desktop_window(&self, _sender: &AnyObject) {
            send(CompositorMessage::RestoreDesktopWindow);
        }

        #[method(startContainerSession:)]
        fn start_container_session(&self, sender: &AnyObject) {
            let payload: Option<Retained<NSString>> = unsafe { msg_send_id![sender, representedObject] };
            let Some(payload) = payload else { return; };
            let Ok(target) = serde_json::from_str::<crate::container_sessions::ProfileActionTarget>(&payload.to_string()) else { return; };
            send(CompositorMessage::StartContainerSessionIfCurrent {
                profile_id: target.id,
                signature: target.signature,
            });
        }

        /// Toggles HiDPI rendering.
        #[method(toggleHiDpi:)]
        fn toggle_hidpi(&self, sender: &AnyObject) {
            let cur: isize = unsafe { msg_send![sender, state] };
            let next: isize = if cur == 1 { 0 } else { 1 };
            unsafe { let _: () = msg_send![sender, setState: next]; }
            send(CompositorMessage::ToggleHiDpi);
        }
    }
);

fn activate_application() {
    if let Some(mtm) = MainThreadMarker::new() {
        unsafe {
            NSApplication::sharedApplication(mtm).activate();
        }
    }
}

fn open_panel(page: crate::container_mode::PanelPage) {
    let Some(tx) = sender() else {
        return;
    };
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    activate_application();
    crate::container_mode::show_page(tx, mtm, page);
}

fn open_diagnostic_logs() {
    let Some(path) = crate::startup::log_directory() else {
        return;
    };
    if let Err(error) = crate::startup::prepare_private_directory(&path) {
        log::error!("Could not open diagnostic logs: {error}");
        return;
    }
    if let Err(error) = std::process::Command::new("/usr/bin/open")
        .arg(&path)
        .spawn()
    {
        log::error!("Could not open {}: {error}", path.display());
    }
}

pub fn remove_status_item(mtm: MainThreadMarker) {
    NATIVE_MENU.with(|state| {
        let mut state = state.borrow_mut();
        if let Some(item) = state.status_item.take() {
            unsafe {
                NSStatusBar::systemStatusBar().removeStatusItem(&item);
            }
        }
        state.handler = None;
    });
    let _ = mtm;
}

// ── Quick-connect dialog ──────────────────────────────────────────────────────

/// Shows a compact form for the same SSH path exposed by run_waypipe.sh.
/// The connection is sent back to the compositor loop so its child is tracked.
unsafe fn show_quick_connect_dialog() {
    use objc2_app_kit::{NSAlert, NSButton, NSSecureTextField, NSTextField, NSView};
    use objc2_foundation::NSRect;

    let mtm = unsafe { MainThreadMarker::new_unchecked() };

    let alert: Retained<NSAlert> = msg_send_id![NSAlert::class(), new];
    let _: () =
        msg_send![&*alert, setMessageText: &*NSString::from_str("Connect to Remote Machine")];
    let _: () = msg_send![&*alert, setInformativeText:
        &*NSString::from_str("Enter the SSH host and Wayland app. Saved entries appear in Connections; passwords are never stored.")];

    // Accessory view containing host, password, app, and an optional display slot.
    // NSView origin is bottom-left, so y increases upward.
    let frame = NSRect {
        origin: objc2_foundation::NSPoint { x: 0.0, y: 0.0 },
        size: objc2_foundation::NSSize {
            width: 340.0,
            height: 224.0,
        },
    };
    let view: Retained<NSView> = msg_send_id![
        msg_send_id![NSView::class(), alloc],
        initWithFrame: frame
    ];

    // Optional friendly name for a reusable connection.
    let name_frame = NSRect {
        origin: objc2_foundation::NSPoint { x: 0.0, y: 188.0 },
        size: objc2_foundation::NSSize {
            width: 340.0,
            height: 28.0,
        },
    };
    let name_field: Retained<NSTextField> = msg_send_id![
        msg_send_id![NSTextField::class(), alloc],
        initWithFrame: name_frame
    ];
    let _: () = msg_send![&*name_field, setPlaceholderString:
        &*NSString::from_str("Connection name (defaults to user@host)")];

    // SSH target: user@host
    let host_frame = NSRect {
        origin: objc2_foundation::NSPoint { x: 0.0, y: 152.0 },
        size: objc2_foundation::NSSize {
            width: 340.0,
            height: 28.0,
        },
    };
    let host_field: Retained<NSTextField> = msg_send_id![
        msg_send_id![NSTextField::class(), alloc],
        initWithFrame: host_frame
    ];
    let _: () = msg_send![&*host_field, setPlaceholderString:
        &*NSString::from_str("user@hostname-or-IP")];

    // Middle field: password (masked)
    let pass_frame = NSRect {
        origin: objc2_foundation::NSPoint { x: 0.0, y: 116.0 },
        size: objc2_foundation::NSSize {
            width: 340.0,
            height: 28.0,
        },
    };
    let pass_field: Retained<NSSecureTextField> = msg_send_id![
        msg_send_id![NSSecureTextField::class(), alloc],
        initWithFrame: pass_frame
    ];
    let _: () = msg_send![&*pass_field, setPlaceholderString:
        &*NSString::from_str("Password (leave blank to use SSH key)")];

    // Bottom field: app to launch
    let prog_frame = NSRect {
        origin: objc2_foundation::NSPoint { x: 0.0, y: 80.0 },
        size: objc2_foundation::NSSize {
            width: 340.0,
            height: 28.0,
        },
    };
    let prog_field: Retained<NSTextField> = msg_send_id![
        msg_send_id![NSTextField::class(), alloc],
        initWithFrame: prog_frame
    ];
    let _: () = msg_send![&*prog_field, setPlaceholderString:
        &*NSString::from_str("App to launch (e.g. niri or foot)")];

    let display_frame = NSRect {
        origin: objc2_foundation::NSPoint { x: 0.0, y: 44.0 },
        size: objc2_foundation::NSSize {
            width: 340.0,
            height: 28.0,
        },
    };
    let display_field: Retained<NSTextField> = msg_send_id![
        msg_send_id![NSTextField::class(), alloc],
        initWithFrame: display_frame
    ];
    let _: () = msg_send![&*display_field, setPlaceholderString:
        &*NSString::from_str("Display slot (blank or default; e.g. display-1)")];

    let save_button = unsafe {
        NSButton::checkboxWithTitle_target_action(
            &NSString::from_str("Save this connection for later"),
            None,
            None,
            mtm,
        )
    };
    let _: () = msg_send![&*save_button, setFrame: NSRect {
        origin: objc2_foundation::NSPoint { x: 0.0, y: 4.0 },
        size: objc2_foundation::NSSize { width: 340.0, height: 28.0 },
    }];

    let _: () = msg_send![&*view, addSubview: &*name_field];
    let _: () = msg_send![&*view, addSubview: &*host_field];
    let _: () = msg_send![&*view, addSubview: &*pass_field];
    let _: () = msg_send![&*view, addSubview: &*prog_field];
    let _: () = msg_send![&*view, addSubview: &*display_field];
    let _: () = msg_send![&*view, addSubview: &*save_button];
    let _: () = msg_send![&*alert, setAccessoryView: &*view];

    let _: Retained<NSObject> = msg_send_id![&*alert, addButtonWithTitle:
        &*NSString::from_str("Connect")];
    let _: Retained<NSObject> = msg_send_id![&*alert, addButtonWithTitle:
        &*NSString::from_str("Cancel")];

    // Make the first field the initial responder
    let _: () = msg_send![&*alert, layout];
    let _win: Retained<NSObject> = msg_send_id![&*alert, window];
    let response: isize = msg_send![&*alert, runModal];
    // NSAlertFirstButtonReturn = 1000
    if response == 1000 {
        let name_ns: Retained<NSString> = msg_send_id![&*name_field, stringValue];
        let host_ns: Retained<NSString> = msg_send_id![&*host_field, stringValue];
        let pass_ns: Retained<NSString> = msg_send_id![&*pass_field, stringValue];
        let prog_ns: Retained<NSString> = msg_send_id![&*prog_field, stringValue];
        let display_ns: Retained<NSString> = msg_send_id![&*display_field, stringValue];
        let name_str = name_ns.to_string().trim().to_string();
        let host_str = host_ns.to_string().trim().to_string();
        let pass_str = pass_ns.to_string();
        let prog_str = prog_ns.to_string().trim().to_string();
        let display_str = display_ns.to_string().trim().to_string();
        let save_state: isize = msg_send![&*save_button, state];
        if !host_str.is_empty() {
            let (user, host_addr) = if let Some(idx) = host_str.find('@') {
                (
                    Some(host_str[..idx].to_string()),
                    host_str[idx + 1..].to_string(),
                )
            } else {
                (None, host_str.clone())
            };
            log::info!("Quick-connect: {} app={}", host_str, prog_str);
            let conn = crate::connections::Connection {
                name: if name_str.is_empty() {
                    host_str
                } else {
                    name_str
                },
                conn_type: "ssh".to_string(),
                host: Some(host_addr),
                user,
                port: None,
                identity: None,
                socket: None,
                app: if prog_str.is_empty() {
                    None
                } else {
                    Some(prog_str)
                },
                display: if display_str.is_empty() {
                    None
                } else {
                    Some(display_str)
                },
                compression: None,
                password: if pass_str.is_empty() {
                    None
                } else {
                    Some(pass_str)
                },
                waypipe_path: None,
            };
            if save_state == 1 {
                match crate::connections::save_connection(&conn) {
                    Ok(_) => {
                        log::info!("Saved connection '{}'", conn.name);
                        send(CompositorMessage::ReloadMenu);
                    }
                    Err(error) => show_connection_save_error(&error, mtm),
                }
            }
            send(CompositorMessage::ConnectMachine(conn));
        } else {
            show_connection_error("Enter a hostname or IP address.", mtm);
        }
    }
}

pub fn show_connection_error(message: &str, mtm: MainThreadMarker) {
    unsafe {
        let alert = NSAlert::new(mtm);
        alert.setMessageText(&NSString::from_str("Connection Failed"));
        alert.setInformativeText(&NSString::from_str(message));
        alert.addButtonWithTitle(&NSString::from_str("OK"));
        alert.runModal();
    }
}

pub fn show_startup_error(message: &str, mtm: MainThreadMarker) {
    unsafe {
        let alert = NSAlert::new(mtm);
        alert.setMessageText(&NSString::from_str("Cocoa-Way Could Not Start"));
        let detail = crate::startup::log_directory().map_or_else(
            || message.to_string(),
            |path| format!("{message}\n\nDiagnostic logs: {}", path.display()),
        );
        alert.setInformativeText(&NSString::from_str(&detail));
        alert.addButtonWithTitle(&NSString::from_str("Quit"));
        alert.addButtonWithTitle(&NSString::from_str("Open Diagnostic Logs"));
        if alert.runModal() == 1001 {
            open_diagnostic_logs();
        }
    }
}

fn show_connection_save_error(message: &str, mtm: MainThreadMarker) {
    unsafe {
        let alert = NSAlert::new(mtm);
        alert.setMessageText(&NSString::from_str("Could Not Save Connection"));
        alert.setInformativeText(&NSString::from_str(message));
        alert.addButtonWithTitle(&NSString::from_str("Continue Without Saving"));
        alert.runModal();
    }
}

fn install_application_icon(app: &NSApplication, mtm: MainThreadMarker) {
    static ICON: &[u8] = include_bytes!("../assets/icon.png");
    let data = NSData::with_bytes(ICON);
    match NSImage::initWithData(mtm.alloc::<NSImage>(), &data) {
        Some(icon) => unsafe { app.setApplicationIconImage(Some(&icon)) },
        None => log::warn!("Failed to decode the embedded Cocoa-Way app icon"),
    }
}

// ── Helper ────────────────────────────────────────────────────────────────────

unsafe fn label_item(title: &str, mtm: MainThreadMarker) -> Retained<NSMenuItem> {
    unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            mtm.alloc::<NSMenuItem>(),
            &NSString::from_str(title),
            None,
            &NSString::from_str(""),
        )
    }
}

// ── Public setup ──────────────────────────────────────────────────────────────

/// Build and install the custom NSApplication main menu.
/// Must be called on the main thread after winit's `applicationDidFinishLaunching`.
pub fn setup_menu(
    connections: &[Connection],
    container_sessions: &[ContainerSession],
    sender: Sender<CompositorMessage>,
    mtm: MainThreadMarker,
) {
    *SENDER.lock().unwrap() = Some(sender);

    unsafe {
        let handler = NATIVE_MENU.with(|state| {
            state
                .borrow_mut()
                .handler
                .get_or_insert_with(|| msg_send_id![MenuHandler::class(), new])
                .clone()
        });

        let app = NSApplication::sharedApplication(mtm);
        install_application_icon(&app, mtm);
        let root = NSMenu::new(mtm);

        // ── 1. App menu ("Cocoa-Way") ─────────────────────────────────────────
        let app_item = NSMenuItem::new(mtm);
        let app_menu = NSMenu::new(mtm);
        let quit = NSMenuItem::initWithTitle_action_keyEquivalent(
            mtm.alloc::<NSMenuItem>(),
            &NSString::from_str("Quit Cocoa-Way"),
            Some(sel!(terminate:)),
            &NSString::from_str("q"),
        );
        app_menu.addItem(&quit);
        app_item.setSubmenu(Some(&app_menu));
        root.addItem(&app_item);

        // ── 2. Connections menu ───────────────────────────────────────────────
        let conn_item = label_item("Connections", mtm);
        let conn_menu =
            NSMenu::initWithTitle(mtm.alloc::<NSMenu>(), &NSString::from_str("Connections"));

        // "Connect to Machine…" dialog item (always present)
        let quick = NSMenuItem::initWithTitle_action_keyEquivalent(
            mtm.alloc::<NSMenuItem>(),
            &NSString::from_str("Connect to Machine…"),
            Some(sel!(quickConnect:)),
            &NSString::from_str("n"),
        );
        let _: () = msg_send![&*quick, setTarget: &*handler];
        conn_menu.addItem(&quick);
        conn_menu.addItem(&NSMenuItem::separatorItem(mtm));

        // Saved connections
        if connections.is_empty() {
            let ph = label_item("No saved connections — use 'Connect to Machine…'", mtm);
            let _: () = msg_send![&*ph, setEnabled: false];
            conn_menu.addItem(&ph);
        } else {
            for (i, conn) in connections.iter().enumerate() {
                let item = NSMenuItem::initWithTitle_action_keyEquivalent(
                    mtm.alloc::<NSMenuItem>(),
                    &NSString::from_str(&conn.name),
                    Some(sel!(connectMachine:)),
                    &NSString::from_str(""),
                );
                let _: () = msg_send![&*item, setTag: i as isize];
                let _: () = msg_send![&*item, setTarget: &*handler];
                conn_menu.addItem(&item);
            }
        }
        conn_menu.addItem(&NSMenuItem::separatorItem(mtm));
        let disconnect = NSMenuItem::initWithTitle_action_keyEquivalent(
            mtm.alloc::<NSMenuItem>(),
            &NSString::from_str("Disconnect Classic Connections"),
            Some(sel!(disconnectClassicConnections:)),
            &NSString::from_str(""),
        );
        let _: () = msg_send![&*disconnect, setTarget: &*handler];
        conn_menu.addItem(&disconnect);
        conn_item.setSubmenu(Some(&conn_menu));
        root.addItem(&conn_item);

        // ── 3. Container menu ─────────────────────────────────────────────────
        let container_item = label_item("Container", mtm);
        let container_menu =
            NSMenu::initWithTitle(mtm.alloc::<NSMenu>(), &NSString::from_str("Container"));

        let open_container = NSMenuItem::initWithTitle_action_keyEquivalent(
            mtm.alloc::<NSMenuItem>(),
            &NSString::from_str("Open Container Mode…"),
            Some(sel!(openContainerMode:)),
            &NSString::from_str(""),
        );
        let _: () = msg_send![&*open_container, setTarget: &*handler];
        container_menu.addItem(&open_container);
        container_menu.addItem(&NSMenuItem::separatorItem(mtm));

        if container_sessions.is_empty() {
            let ph = label_item("No container sessions configured", mtm);
            let _: () = msg_send![&*ph, setEnabled: false];
            container_menu.addItem(&ph);
        } else {
            for (i, session) in container_sessions.iter().enumerate() {
                let item = NSMenuItem::initWithTitle_action_keyEquivalent(
                    mtm.alloc::<NSMenuItem>(),
                    &NSString::from_str(&session.name),
                    Some(sel!(startContainerSession:)),
                    &NSString::from_str(""),
                );
                let _: () = msg_send![&*item, setTag: i as isize];
                let _: () = msg_send![&*item, setRepresentedObject:
                    &*NSString::from_str(&serde_json::to_string(&crate::container_sessions::ProfileActionTarget::capture(session)).unwrap())];
                let _: () = msg_send![&*item, setTarget: &*handler];
                container_menu.addItem(&item);
            }
        }

        container_item.setSubmenu(Some(&container_menu));
        root.addItem(&container_item);

        // ── 4. View menu ──────────────────────────────────────────────────────
        let view_item = label_item("View", mtm);
        let view_menu = NSMenu::initWithTitle(mtm.alloc::<NSMenu>(), &NSString::from_str("View"));
        let show_display = NSMenuItem::initWithTitle_action_keyEquivalent(
            mtm.alloc::<NSMenuItem>(),
            &NSString::from_str("Show Default Display"),
            Some(sel!(showDefaultDisplay:)),
            &NSString::from_str("1"),
        );
        let _: () = msg_send![&*show_display, setTarget: &*handler];
        view_menu.addItem(&show_display);
        view_menu.addItem(&NSMenuItem::separatorItem(mtm));
        let hidpi = NSMenuItem::initWithTitle_action_keyEquivalent(
            mtm.alloc::<NSMenuItem>(),
            &NSString::from_str("HiDPI Display"),
            Some(sel!(toggleHiDpi:)),
            &NSString::from_str(""),
        );
        let _: () = msg_send![&*hidpi, setState: 0isize]; // unchecked: Normal 1x is default
        let _: () = msg_send![&*hidpi, setTarget: &*handler];
        view_menu.addItem(&hidpi);
        view_menu.addItem(&NSMenuItem::separatorItem(mtm));
        let desktop_background = NSMenuItem::initWithTitle_action_keyEquivalent(
            mtm.alloc::<NSMenuItem>(),
            &NSString::from_str("Use Desktop Background (Experimental)"),
            Some(sel!(toggleDesktopBackground:)),
            &NSString::from_str(""),
        );
        let _: () = msg_send![&*desktop_background, setTarget: &*handler];
        DESKTOP_BACKGROUND_MENU.with(|state| {
            let mut state = state.borrow_mut();
            let _: () = msg_send![&*desktop_background, setState: isize::from(state.active)];
            state.items = vec![desktop_background.clone()];
        });
        view_menu.addItem(&desktop_background);
        let restore_desktop = NSMenuItem::initWithTitle_action_keyEquivalent(
            mtm.alloc::<NSMenuItem>(),
            &NSString::from_str("Restore Desktop Window"),
            Some(sel!(restoreDesktopWindow:)),
            &NSString::from_str("b"),
        );
        restore_desktop.setKeyEquivalentModifierMask(
            NSEventModifierFlags::NSEventModifierFlagCommand
                | NSEventModifierFlags::NSEventModifierFlagOption,
        );
        let _: () = msg_send![&*restore_desktop, setTarget: &*handler];
        // Keep the escape action available even when desktop mode is off.
        let _: () = msg_send![&*restore_desktop, setEnabled: true];
        view_menu.addItem(&restore_desktop);
        view_item.setSubmenu(Some(&view_menu));
        root.addItem(&view_item);

        app.setMainMenu(Some(&root));
        install_status_menu(connections, container_sessions, &handler, &app, mtm);
    }
}

unsafe fn action_item(
    title: &str,
    action: objc2::runtime::Sel,
    handler: &MenuHandler,
    mtm: MainThreadMarker,
) -> Retained<NSMenuItem> {
    let item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            mtm.alloc::<NSMenuItem>(),
            &NSString::from_str(title),
            Some(action),
            &NSString::from_str(""),
        )
    };
    unsafe {
        item.setTarget(Some(handler));
    }
    item
}

unsafe fn install_status_menu(
    connections: &[Connection],
    sessions: &[ContainerSession],
    handler: &MenuHandler,
    app: &NSApplication,
    mtm: MainThreadMarker,
) {
    let menu = NSMenu::new(mtm);
    unsafe {
        menu.setAutoenablesItems(false);
    }
    let title = unsafe { label_item(&format!("Cocoa-Way {}", env!("CARGO_PKG_VERSION")), mtm) };
    unsafe {
        title.setEnabled(false);
    }
    menu.addItem(&title);
    menu.addItem(&NSMenuItem::separatorItem(mtm));
    for (title, action) in [
        ("Applications...", sel!(openApplications:)),
        ("Connect to Machine...", sel!(quickConnect:)),
        ("Displays...", sel!(openDisplays:)),
        ("Diagnostics...", sel!(openDiagnostics:)),
        ("Check Environment...", sel!(checkEnvironment:)),
        ("Open Diagnostic Logs", sel!(openDiagnosticLogs:)),
    ] {
        let item = unsafe { action_item(title, action, handler, mtm) };
        menu.addItem(&item);
    }
    if !connections.is_empty() {
        let submenu = NSMenu::new(mtm);
        for (index, connection) in connections.iter().enumerate() {
            let item =
                unsafe { action_item(&connection.name, sel!(connectMachine:), handler, mtm) };
            unsafe {
                item.setTag(index as isize);
            }
            submenu.addItem(&item);
        }
        let saved = unsafe { label_item("Saved Connections", mtm) };
        saved.setSubmenu(Some(&submenu));
        menu.addItem(&saved);
    }
    if !sessions.is_empty() {
        let submenu = NSMenu::new(mtm);
        for (index, session) in sessions.iter().enumerate() {
            let item =
                unsafe { action_item(&session.name, sel!(startContainerSession:), handler, mtm) };
            unsafe {
                item.setTag(index as isize);
                let _: () = msg_send![&*item, setRepresentedObject:
                    &*NSString::from_str(&serde_json::to_string(&crate::container_sessions::ProfileActionTarget::capture(session)).unwrap())];
            }
            submenu.addItem(&item);
        }
        let applications = unsafe { label_item("Launch Application", mtm) };
        applications.setSubmenu(Some(&submenu));
        menu.addItem(&applications);
    }
    menu.addItem(&NSMenuItem::separatorItem(mtm));
    for (title, action) in [
        ("Show Default Display", sel!(showDefaultDisplay:)),
        ("Hide Default Display", sel!(hideDefaultDisplay:)),
        (
            "Use Desktop Background (Experimental)",
            sel!(toggleDesktopBackground:),
        ),
        ("Restore Desktop Window", sel!(restoreDesktopWindow:)),
    ] {
        let item = unsafe { action_item(title, action, handler, mtm) };
        if action == sel!(toggleDesktopBackground:) {
            DESKTOP_BACKGROUND_MENU.with(|state| {
                let mut state = state.borrow_mut();
                unsafe {
                    let _: () = msg_send![&*item, setState: isize::from(state.active)];
                }
                state.items.push(item.clone());
            });
        }
        menu.addItem(&item);
    }
    menu.addItem(&NSMenuItem::separatorItem(mtm));
    let quit = unsafe { action_item("Quit Cocoa-Way", sel!(terminate:), handler, mtm) };
    unsafe {
        quit.setTarget(Some(app));
    }
    menu.addItem(&quit);
    NATIVE_MENU.with(|state| {
        let mut state = state.borrow_mut();
        let item = state.status_item.get_or_insert_with(|| unsafe {
            NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength)
        });
        unsafe {
            if let Some(button) = item.button(mtm) {
                if let Some(image) = NSImage::imageWithSystemSymbolName_accessibilityDescription(
                    &NSString::from_str("display"),
                    Some(&NSString::from_str("Cocoa-Way")),
                ) {
                    image.setTemplate(true);
                    image.setSize(objc2_foundation::NSSize {
                        width: 18.0,
                        height: 18.0,
                    });
                    button.setImage(Some(&image));
                    button.setTitle(&NSString::from_str(""));
                } else {
                    button.setTitle(&NSString::from_str("CW"));
                }
                button.setToolTip(Some(&NSString::from_str(
                    "Cocoa-Way: applications, connections and diagnostics",
                )));
            }
            item.setMenu(Some(&menu));
        }
    });
}

/// Disable macOS window tab bar (removes "Show Tab Bar" from the View menu).
/// Call this once after the winit window is created.
pub fn disable_window_tabbing(ns_window_ptr: *mut std::ffi::c_void) {
    if ns_window_ptr.is_null() {
        return;
    }
    unsafe {
        let win = ns_window_ptr as *mut AnyObject;
        // NSWindowTabbingModeDisallowed = 2
        let _: () = msg_send![win, setTabbingMode: 2isize];
    }
}
