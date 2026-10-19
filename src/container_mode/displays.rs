use std::cell::{Cell, RefCell};
use std::collections::HashSet;

use objc2::rc::Retained;
use objc2::{msg_send, sel};
use objc2_app_kit::{
    NSAutoresizingMaskOptions, NSButton, NSLayoutAttribute as Attribute, NSStackView, NSView,
    NSWindow,
};
use objc2_foundation::{MainThreadMarker, NSString};

use super::native_layout::{self as ui, ResponsiveActions, ScrollStack};
use super::*;

thread_local! {
    static PAGE: RefCell<Option<DisplaysPage>> = const { RefCell::new(None) };
    static OPEN_DETAILS: RefCell<HashSet<String>> = RefCell::new(HashSet::new());
    static LAYOUT_ACTIVE: Cell<bool> = const { Cell::new(false) };
}

struct DetailToggle {
    key: String,
    button: Retained<NSButton>,
    body: Retained<NSStackView>,
}

struct DisplaysPage {
    root: Retained<NSView>,
    sidebar: ScrollStack,
    content: ScrollStack,
    details: Vec<DetailToggle>,
}

pub(super) fn clear() {
    PAGE.with(|page| {
        page.replace(None);
    });
}

pub(super) fn relayout() -> bool {
    LAYOUT_ACTIVE.with(|active| {
        if active.replace(true) {
            return true;
        }
        struct Reset<'a>(&'a Cell<bool>);
        impl Drop for Reset<'_> {
            fn drop(&mut self) {
                self.0.set(false);
            }
        }
        let _reset = Reset(active);
        PAGE.with(|page| {
            // AppKit may synchronously notify the delegate during a constraint pass.
            let Ok(page) = page.try_borrow() else {
                return true;
            };
            let Some(page) = page.as_ref() else {
                return false;
            };
            unsafe {
                page.root.layoutSubtreeIfNeeded();
            }
            page.sidebar.reflow();
            page.content.reflow();
            true
        })
    })
}

pub(super) fn toggle_details(index: usize) {
    PAGE.with(|page| {
        let Ok(page) = page.try_borrow() else {
            return;
        };
        let Some(toggle) = page.as_ref().and_then(|page| page.details.get(index)) else {
            return;
        };
        let open = OPEN_DETAILS.with(|open| {
            let mut open = open.borrow_mut();
            if open.remove(&toggle.key) {
                false
            } else {
                open.insert(toggle.key.clone());
                true
            }
        });
        unsafe {
            toggle.body.setHidden(!open);
            let title = if open {
                "Hide connection details"
            } else {
                "Connection details"
            };
            let _: () = msg_send![&*toggle.button, setTitle: &*NSString::from_str(&tr(title))];
        }
    });
    relayout();
}

pub(super) fn install(
    window: &NSWindow,
    sessions: &[ContainerSession],
    config_error: Option<&str>,
    handler: *mut AnyObject,
    mtm: MainThreadMarker,
) {
    let (width, height) = content_size(window);
    let root = ui::view(mtm);
    unsafe {
        root.setFrame(rect(0.0, 0.0, width, height));
        root.setAutoresizingMask(
            NSAutoresizingMaskOptions::NSViewWidthSizable
                | NSAutoresizingMaskOptions::NSViewHeightSizable,
        );
    }
    let sidebar_host = add_sidebar_background(&root, panel_shell::SIDEBAR_WIDTH, height, mtm);
    let sidebar = panel_shell::sidebar(&sidebar_host, sessions.len(), NAV_DISPLAYS, handler, mtm);

    let main = ui::view(mtm);
    ui::attach(&root, &main);
    ui::constrain(
        &main,
        Attribute::Leading,
        Some((&root, Attribute::Leading)),
        panel_shell::SIDEBAR_WIDTH,
    );
    ui::constrain(
        &main,
        Attribute::Trailing,
        Some((&root, Attribute::Trailing)),
        0.0,
    );
    ui::constrain(&main, Attribute::Top, Some((&root, Attribute::Top)), 0.0);
    ui::constrain(
        &main,
        Attribute::Bottom,
        Some((&root, Attribute::Bottom)),
        0.0,
    );

    let header = ui::view(mtm);
    ui::attach(&main, &header);
    ui::constrain(
        &header,
        Attribute::Leading,
        Some((&main, Attribute::Leading)),
        0.0,
    );
    ui::constrain(
        &header,
        Attribute::Trailing,
        Some((&main, Attribute::Trailing)),
        0.0,
    );
    ui::constrain(&header, Attribute::Top, Some((&main, Attribute::Top)), 0.0);
    ui::constrain(&header, Attribute::Height, None, panel_shell::HEADER_HEIGHT);
    panel_shell::populate_header(&header, NAV_DISPLAYS, handler, mtm);

    let footer = ui::view(mtm);
    ui::attach(&main, &footer);
    ui::constrain(
        &footer,
        Attribute::Leading,
        Some((&main, Attribute::Leading)),
        0.0,
    );
    ui::constrain(
        &footer,
        Attribute::Trailing,
        Some((&main, Attribute::Trailing)),
        0.0,
    );
    ui::constrain(
        &footer,
        Attribute::Bottom,
        Some((&main, Attribute::Bottom)),
        0.0,
    );
    ui::constrain(&footer, Attribute::Height, None, panel_shell::FOOTER_HEIGHT);
    panel_shell::populate_footer(&footer, mtm);

    let mut content = ScrollStack::new(24.0, mtm);
    ui::attach(&main, &content.scroll);
    ui::constrain(
        &content.scroll,
        Attribute::Leading,
        Some((&main, Attribute::Leading)),
        0.0,
    );
    ui::constrain(
        &content.scroll,
        Attribute::Trailing,
        Some((&main, Attribute::Trailing)),
        0.0,
    );
    ui::constrain(
        &content.scroll,
        Attribute::Top,
        Some((&header, Attribute::Bottom)),
        0.0,
    );
    ui::constrain(
        &content.scroll,
        Attribute::Bottom,
        Some((&footer, Attribute::Top)),
        0.0,
    );

    let mut details = Vec::new();
    populate(
        &mut content,
        &mut details,
        sessions,
        config_error,
        handler,
        mtm,
    );
    let scroll_pointer = Retained::as_ptr(&content.scroll) as usize;
    PAGE.with(|page| {
        page.replace(Some(DisplaysPage {
            root: root.clone(),
            sidebar,
            content,
            details,
        }));
    });
    *DETAIL_SCROLL_VIEW.lock().unwrap() = None;
    *LIST_SCROLL_VIEW.lock().unwrap() = Some(TrackedScrollView {
        pointer: scroll_pointer,
        key: "list:3".into(),
    });
    window.setContentView(Some(&root));
    relayout();
    PAGE.with(|page| {
        if let Some(page) = page.borrow().as_ref() {
            let distance = SCROLL_POSITIONS
                .lock()
                .unwrap()
                .get("list:3")
                .copied()
                .unwrap_or(0.0);
            page.content.restore(distance);
        }
    });
}

fn section(content: &mut ScrollStack, title: &str, detail: &str, mtm: MainThreadMarker) {
    let section = ui::stack(true, 5.0, mtm);
    let title = content.label(&tr(title), TextStyle::Heading, mtm);
    let detail = content.label(&tr(detail), TextStyle::Caption, mtm);
    ui::arranged(&section, &title, true);
    ui::arranged(&section, &detail, true);
    ui::arranged(&content.body, &section, true);
}

fn card_body(
    content: &mut ScrollStack,
    title: &str,
    detail: &str,
    icon: &str,
    mtm: MainThreadMarker,
) -> Retained<NSStackView> {
    let body = ui::stack(true, 12.0, mtm);
    let heading = ui::stack(false, 10.0, mtm);
    let icon = ui::symbol(icon, title, mtm);
    let title = content.label(title, TextStyle::Heading, mtm);
    ui::arranged(&heading, &icon, false);
    ui::arranged(&heading, &title, false);
    ui::arranged(&body, &heading, true);
    if !detail.is_empty() {
        let detail = content.label(detail, TextStyle::Body, mtm);
        ui::arranged(&body, &detail, true);
    }
    body
}

fn add_card(content: &ScrollStack, body: &NSStackView, mtm: MainThreadMarker) {
    let card = ui::card(body, mtm);
    ui::arranged(&content.body, &card, true);
}

fn badge(body: &NSStackView, status: DisplayStatus, mtm: MainThreadMarker) {
    let label = ui::single_line(&tr(status.label()), TextStyle::Caption, mtm);
    let color = unsafe {
        match status {
            DisplayStatus::Attached => NSColor::systemBlueColor(),
            DisplayStatus::Allocating | DisplayStatus::Closing => NSColor::systemOrangeColor(),
            DisplayStatus::Failed => NSColor::systemRedColor(),
            _ => NSColor::systemGreenColor(),
        }
    };
    unsafe {
        let _: () = msg_send![&*label, setTextColor: &*color];
    }
    ui::arranged(body, &label, false);
}

fn actions(
    content: &mut ScrollStack,
    body: &NSStackView,
    buttons: &[Retained<NSButton>],
    mtm: MainThreadMarker,
) {
    let row = ResponsiveActions::new(buttons, mtm);
    ui::arranged(body, &row.stack, true);
    content.actions.push(row);
}

fn telemetry(
    body: &NSStackView,
    slot: &str,
    snapshot: Option<&PerformanceSnapshot>,
    mtm: MainThreadMarker,
) {
    let label = ui::single_line(
        &display_fps_text("Display", snapshot),
        TextStyle::Caption,
        mtm,
    );
    ui::arranged(body, &label, true);
    register_live_display_fps_label(&label, slot, "Display");
}

fn populate(
    content: &mut ScrollStack,
    details: &mut Vec<DetailToggle>,
    sessions: &[ContainerSession],
    config_error: Option<&str>,
    handler: *mut AnyObject,
    mtm: MainThreadMarker,
) {
    MANAGED_DISPLAY_ACTIONS.lock().unwrap().clear();
    let managed = managed_displays_snapshot();
    let pending = pending_managed_displays_snapshot();
    let closing = closing_managed_displays_snapshot();
    let active = active_sessions_snapshot();
    OPEN_DETAILS.with(|open| {
        open.borrow_mut()
            .retain(|slot| managed.iter().any(|display| &display.slot == slot))
    });
    let last_error = MANAGED_DISPLAY_LAST_ERROR.lock().unwrap().clone();
    for (title, error) in [
        ("Display operation failed", last_error.as_deref()),
        ("Application configuration needs attention", config_error),
    ] {
        if let Some(error) = error {
            let body = card_body(content, &tr(title), error, "exclamationmark.triangle", mtm);
            badge(&body, DisplayStatus::Failed, mtm);
            add_card(content, &body, mtm);
        }
    }
    section(
        content,
        "Display windows",
        "Auto uses the built-in window when available. Create a named window for an explicit script or application connection.",
        mtm,
    );
    let default_count = active
        .iter()
        .filter(|session| session.instance.display_slot == "default")
        .count();
    let built_in = card_body(
        content,
        &tr("Built-in display"),
        &tr(
            "The shared Cocoa-Way desktop window. Other auto sessions receive independent displays when this one is occupied.",
        ),
        "desktopcomputer",
        mtm,
    );
    badge(
        &built_in,
        if default_count == 0 {
            DisplayStatus::Free
        } else {
            DisplayStatus::Attached
        },
        mtm,
    );
    telemetry(&built_in, "default", performance_snapshot().as_ref(), mtm);
    add_card(content, &built_in, mtm);
    for slot in pending {
        let body = card_body(
            content,
            &slot,
            &tr("Starting an independent Wayland window..."),
            "display",
            mtm,
        );
        badge(&body, DisplayStatus::Allocating, mtm);
        add_card(content, &body, mtm);
    }
    for display in managed {
        let attached = active
            .iter()
            .filter(|active| active.instance.display_slot == display.slot)
            .count();
        let is_closing = closing.contains(&display.slot);
        let status = if is_closing {
            DisplayStatus::Closing
        } else if attached > 0 {
            DisplayStatus::Attached
        } else {
            DisplayStatus::Free
        };
        let action_index = {
            let mut registry = MANAGED_DISPLAY_ACTIONS.lock().unwrap();
            let index = registry.len();
            registry.push(display.clone());
            index
        };
        let body = card_body(
            content,
            &display.slot,
            &tr_format(
                "{count} managed application attachments",
                &[("count", &attached.to_string())],
            ),
            "display",
            mtm,
        );
        badge(&body, status, mtm);
        telemetry(
            &body,
            &display.slot,
            worker_performance_snapshot(&display.runtime_dir).as_ref(),
            mtm,
        );
        let focus = ui::button(
            &tr("Focus Window"),
            handler,
            sel!(focusManagedDisplay:),
            action_index,
            mtm,
        );
        let copy = ui::button(
            &tr("Copy Connection"),
            handler,
            sel!(copyManagedDisplayCommand:),
            action_index,
            mtm,
        );
        let close = ui::button(
            &tr("Close Display"),
            handler,
            sel!(closeManagedDisplay:),
            action_index,
            mtm,
        );
        unsafe {
            let _: () = msg_send![&*focus, setEnabled: !is_closing];
            let _: () = msg_send![&*copy, setEnabled: !is_closing];
            let _: () = msg_send![&*close, setEnabled: !is_closing && attached == 0];
            let _: () = msg_send![&*copy, setToolTip: &*NSString::from_str(&tr("Copy the run_waypipe.sh command prefix for this display"))];
            let _: () = msg_send![&*close, setToolTip: &*NSString::from_str(if attached > 0 { "Stop managed attachments before closing this display" } else { "Close this display, including any external script clients" })];
        }
        actions(content, &body, &[focus, copy, close], mtm);
        let detail_body = ui::stack(true, 6.0, mtm);
        let socket = content.label(
            &tr_format(
                "Socket\n{path}",
                &[(
                    "path",
                    &format!("{}/{}", display.runtime_dir, display.display),
                )],
            ),
            TextStyle::Mono,
            mtm,
        );
        let process = content.label(
            &tr_format(
                "Display process {pid}",
                &[("pid", &display.pid.to_string())],
            ),
            TextStyle::Caption,
            mtm,
        );
        ui::arranged(&detail_body, &socket, true);
        ui::arranged(&detail_body, &process, true);
        let open = OPEN_DETAILS.with(|open| open.borrow().contains(&display.slot));
        let toggle = ui::button(
            &tr(if open {
                "Hide connection details"
            } else {
                "Connection details"
            }),
            handler,
            sel!(toggleDisplayConnectionDetails:),
            details.len(),
            mtm,
        );
        unsafe {
            toggle.setBordered(false);
            detail_body.setHidden(!open);
        }
        ui::arranged(&body, &toggle, false);
        ui::arranged(&body, &detail_body, true);
        details.push(DetailToggle {
            key: display.slot,
            button: toggle,
            body: detail_body,
        });
        add_card(content, &body, mtm);
    }

    section(
        content,
        "Current connections",
        "Managed application instances and the displays they own. External script clients are not included in this list.",
        mtm,
    );
    if active.is_empty() {
        let body = card_body(
            content,
            &tr("No managed applications connected"),
            &tr(
                "Launch an application from Applications, or use Copy Connection on a named display for an external client.",
            ),
            "rectangle.on.rectangle",
            mtm,
        );
        add_card(content, &body, mtm);
    }
    for active in active {
        let name = sessions
            .iter()
            .find(|session| session.id == active.profile_id)
            .map(|session| session.name.as_str())
            .unwrap_or("Unknown application");
        let body = card_body(
            content,
            name,
            &tr_format(
                "Instance #{id} on {display}",
                &[
                    ("id", &active.instance.id.to_string()),
                    ("display", &active.instance.display_slot),
                ],
            ),
            "app",
            mtm,
        );
        telemetry(
            &body,
            &active.instance.display_slot,
            performance_for_active_session(&active).as_ref(),
            mtm,
        );
        let release = ui::button(
            &tr("Stop and Release"),
            handler,
            sel!(releaseDisplayAttachment:),
            0,
            mtm,
        );
        unsafe {
            let _: () = msg_send![&*release, setIdentifier: &*NSString::from_str(&active.profile_id.to_string())];
            let _: () = msg_send![&*release, setToolTip: &*NSString::from_str(&tr("Stop this managed application and release its display"))];
        }
        actions(content, &body, &[release], mtm);
        add_card(content, &body, mtm);
    }
    section(
        content,
        "Application policies",
        "Saved preferences take effect when an application launches; they do not reserve a window.",
        mtm,
    );
    if sessions.is_empty() {
        let body = card_body(
            content,
            &tr("No application policies"),
            &tr("Create an application profile to choose its display policy."),
            "app.dashed",
            mtm,
        );
        add_card(content, &body, mtm);
    }
    for session in sessions {
        let body = card_body(
            content,
            &session.name,
            &display_policy_description(session),
            if session.presentation_mode().is_rootless() {
                "macwindow.on.rectangle"
            } else {
                "display"
            },
            mtm,
        );
        add_card(content, &body, mtm);
    }
}
