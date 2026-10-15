use objc2::ClassType;
use objc2::rc::Retained;
use objc2::{msg_send, msg_send_id, sel};
use objc2_app_kit::{
    NSAutoresizingMaskOptions, NSColor, NSLayoutAttribute as Attribute, NSStackView,
    NSStackViewDistribution, NSView,
};
use objc2_foundation::{MainThreadMarker, NSRect, NSString};

use super::native_layout::{self as ui, ScrollStack};
use super::*;

pub(super) fn sidebar(
    parent: &NSView,
    session_count: usize,
    selected_nav: usize,
    handler: *mut AnyObject,
    mtm: MainThreadMarker,
) -> ScrollStack {
    let brand = ui::single_line("Cocoa-Way", TextStyle::Heading, mtm);
    ui::attach(parent, &brand);
    ui::constrain(
        &brand,
        Attribute::Leading,
        Some((parent, Attribute::Leading)),
        18.0,
    );
    ui::constrain(&brand, Attribute::Top, Some((parent, Attribute::Top)), 24.0);
    let settings = ui::button("Settings...", handler, sel!(openContainerSettings:), 0, mtm);
    ui::attach(parent, &settings);
    ui::constrain(
        &settings,
        Attribute::Leading,
        Some((parent, Attribute::Leading)),
        16.0,
    );
    ui::constrain(
        &settings,
        Attribute::Bottom,
        Some((parent, Attribute::Bottom)),
        -18.0,
    );
    let mut menu = ScrollStack::new(14.0, mtm);
    ui::attach(parent, &menu.scroll);
    ui::constrain(
        &menu.scroll,
        Attribute::Leading,
        Some((parent, Attribute::Leading)),
        0.0,
    );
    ui::constrain(
        &menu.scroll,
        Attribute::Trailing,
        Some((parent, Attribute::Trailing)),
        0.0,
    );
    ui::constrain(
        &menu.scroll,
        Attribute::Top,
        Some((&brand, Attribute::Bottom)),
        18.0,
    );
    ui::constrain(
        &menu.scroll,
        Attribute::Bottom,
        Some((&settings, Attribute::Top)),
        -12.0,
    );
    for (section, entries) in [
        (
            "Applications",
            vec![
                (NAV_SESSIONS, "Applications", "app"),
                (NAV_RUNNING, "Running", "play.circle"),
            ],
        ),
        (
            "Resources",
            vec![
                (NAV_IMAGES, "Images", "square.stack"),
                (NAV_VOLUMES, "Volumes", "externaldrive"),
                (NAV_DISPLAYS, "Displays", "display.2"),
            ],
        ),
        (
            "Runtimes",
            vec![
                (NAV_APPLE_CONTAINER, "Apple Container", "shippingbox"),
                (NAV_DOCKER, "Docker-compatible", "cube"),
            ],
        ),
        (
            "Diagnostics",
            vec![
                (NAV_ACTIVITY, "Activity", "waveform.path.ecg"),
                (NAV_COMMANDS, "Commands", "terminal"),
            ],
        ),
    ] {
        let group = ui::stack(true, 5.0, mtm);
        let heading = menu.label(section, TextStyle::Section, mtm);
        ui::arranged(&group, &heading, true);
        for (nav, title, symbol) in entries {
            let button = ui::button(title, handler, sel!(selectContainerNav:), nav, mtm);
            unsafe {
                button.setBordered(false);
                let _: () = msg_send![&*button, setAlignment: 0isize];
                let image: Option<Retained<objc2_app_kit::NSImage>> = msg_send_id![objc2_app_kit::NSImage::class(), imageWithSystemSymbolName: &*NSString::from_str(symbol), accessibilityDescription: &*NSString::from_str(title)];
                let _: () = msg_send![&*button, setImage: image.as_deref()];
                let _: () = msg_send![&*button, setImagePosition: 2isize];
                if nav == selected_nav {
                    let _: () =
                        msg_send![&*button, setContentTintColor: &*NSColor::controlAccentColor()];
                }
                let tooltip = if nav == NAV_SESSIONS {
                    format!("{session_count} saved applications")
                } else {
                    title.to_string()
                };
                let _: () = msg_send![&*button, setToolTip: &*NSString::from_str(&tooltip)];
            }
            ui::arranged(&group, &button, true);
        }
        ui::arranged(&menu.body, &group, true);
    }
    menu
}

pub(super) fn summary(mtm: MainThreadMarker) -> Retained<NSStackView> {
    let footer = ui::stack(false, 16.0, mtm);
    unsafe {
        footer.setDistribution(NSStackViewDistribution::FillEqually);
    }
    for diagnostic in runtime_diagnostics(&[]) {
        let item = ui::stack(true, 4.0, mtm);
        let name = ui::single_line(diagnostic.name, TextStyle::Caption, mtm);
        let value = ui::single_line(&diagnostic.state, TextStyle::Heading, mtm);
        if diagnostic.name == "Display FPS" {
            *SUMMARY_FPS_LABEL.lock().unwrap() = Some(Retained::as_ptr(&value) as usize);
        }
        ui::arranged(&item, &name, true);
        ui::arranged(&item, &value, true);
        ui::arranged(&footer, &item, false);
    }
    footer
}

pub(super) const SIDEBAR_WIDTH: f64 = 212.0;
pub(super) const HEADER_HEIGHT: f64 = 96.0;
pub(super) const FOOTER_HEIGHT: f64 = 72.0;

pub(super) fn header(
    parent: &NSView,
    frame: NSRect,
    nav: usize,
    handler: *mut AnyObject,
    mtm: MainThreadMarker,
) {
    let host = framed_host(parent, frame, mtm);
    populate_header(&host, nav, handler, mtm);
}

pub(super) fn populate_header(
    parent: &NSView,
    nav: usize,
    handler: *mut AnyObject,
    mtm: MainThreadMarker,
) {
    let heading = ui::single_line(nav_title(nav), TextStyle::Title, mtm);
    let actions = ui::stack(false, 8.0, mtm);
    let primary = match nav {
        NAV_SESSIONS => Some(("New Application", sel!(addContainerSession:))),
        NAV_DISPLAYS => Some(("Create Display", sel!(createManagedDisplay:))),
        NAV_ACTIVITY => Some(("Clear", sel!(clearContainerActivity:))),
        _ => None,
    };
    if let Some((title, action)) = primary {
        let button = ui::button(title, handler, action, 0, mtm);
        if matches!(nav, NAV_SESSIONS | NAV_DISPLAYS) {
            unsafe {
                let _: () = msg_send![&*button, setBezelColor: &*NSColor::controlAccentColor()];
            }
        }
        ui::arranged(&actions, &button, false);
    }
    let reload = ui::button("Reload", handler, sel!(reloadContainerMode:), 0, mtm);
    ui::arranged(&actions, &reload, false);
    let toolbar = ui::toolbar(&heading, &actions, mtm);
    pin_inset(parent, &toolbar, 20.0, 14.0);
}

pub(super) fn footer(parent: &NSView, frame: NSRect, mtm: MainThreadMarker) {
    let host = framed_host(parent, frame, mtm);
    populate_footer(&host, mtm);
    unsafe {
        host.setAutoresizingMask(NSAutoresizingMaskOptions::NSViewWidthSizable);
    }
}

pub(super) fn populate_footer(parent: &NSView, mtm: MainThreadMarker) {
    let footer = summary(mtm);
    pin_inset(parent, &footer, 24.0, 16.0);
}

pub(super) fn framed_host(
    parent: &NSView,
    frame: NSRect,
    mtm: MainThreadMarker,
) -> Retained<NSView> {
    let host = ui::view(mtm);
    unsafe {
        host.setFrame(frame);
        parent.addSubview(&host);
    }
    host
}

pub(super) fn detail_header(
    parent: &NSView,
    frame: NSRect,
    selected_tab: usize,
    application: bool,
    active_index: Option<usize>,
    handler: *mut AnyObject,
    mtm: MainThreadMarker,
) {
    let host = framed_host(parent, frame, mtm);
    if !application {
        let heading = ui::single_line("Details", TextStyle::Title, mtm);
        ui::attach(&host, &heading);
        ui::constrain(
            &heading,
            Attribute::Leading,
            Some((&host, Attribute::Leading)),
            20.0,
        );
        ui::constrain(
            &heading,
            Attribute::Trailing,
            Some((&host, Attribute::Trailing)),
            -20.0,
        );
        ui::constrain(
            &heading,
            Attribute::Top,
            Some((&host, Attribute::Top)),
            14.0,
        );
        return;
    }
    let tabs: Retained<NSView> = unsafe {
        msg_send_id![
            objc2::runtime::AnyClass::get("NSSegmentedControl").expect("AppKit segmented control"),
            new
        ]
    };
    unsafe {
        let _: () = msg_send![&*tabs, setSegmentCount: 4isize];
        for (index, title) in ["Overview", "Logs", "Terminal", "Files"].iter().enumerate() {
            let _: () =
                msg_send![&*tabs, setLabel: &*NSString::from_str(title) forSegment: index as isize];
        }
        let _: () = msg_send![&*tabs, setSelectedSegment: selected_tab.min(3) as isize];
        let _: () = msg_send![&*tabs, setTarget: &*handler];
        let _: () = msg_send![&*tabs, setAction: Some(sel!(selectContainerNativeTab:))];
    }
    let actions = ui::stack(false, 8.0, mtm);
    let terminal = ui::button(
        "New Terminal",
        handler,
        sel!(openContainerTerminal:),
        active_index.unwrap_or(0),
        mtm,
    );
    unsafe {
        terminal.setEnabled(active_index.is_some());
        let _: () = msg_send![&*terminal, setToolTip: &*NSString::from_str(
            if active_index.is_some() { "Open a terminal in the running application instance" }
            else { "Launch an application instance before opening its terminal" }
        )];
    }
    ui::arranged(&actions, &terminal, false);
    let toolbar = ui::toolbar(&tabs, &actions, mtm);
    pin_inset(&host, &toolbar, 20.0, 14.0);
}

pub(super) fn pin_inset(parent: &NSView, child: &NSView, horizontal: f64, vertical: f64) {
    ui::attach(parent, child);
    ui::constrain(
        child,
        Attribute::Leading,
        Some((parent, Attribute::Leading)),
        horizontal,
    );
    ui::constrain(
        child,
        Attribute::Trailing,
        Some((parent, Attribute::Trailing)),
        -horizontal,
    );
    ui::constrain(
        child,
        Attribute::Top,
        Some((parent, Attribute::Top)),
        vertical,
    );
    ui::constrain(
        child,
        Attribute::Bottom,
        Some((parent, Attribute::Bottom)),
        -vertical,
    );
}
