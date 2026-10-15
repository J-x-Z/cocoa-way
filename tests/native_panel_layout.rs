#![allow(dead_code)]
#[path = "../src/container_mode/native_layout.rs"]
mod native_layout;

use native_layout::{self as ui, ResponsiveActions, ScrollStack, TextStyle};
use objc2::rc::Retained;
use objc2::{msg_send, msg_send_id};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSButton,
    NSLayoutAttribute as Attribute, NSUserInterfaceLayoutOrientation, NSWindow, NSWindowStyleMask,
};
use objc2_foundation::{MainThreadMarker, NSPoint, NSRect, NSSize, NSString};

fn frame(width: f64, height: f64) -> NSRect {
    NSRect {
        origin: NSPoint { x: 0.0, y: 0.0 },
        size: NSSize { width, height },
    }
}

fn main() {
    let mtm =
        MainThreadMarker::new().expect("Native layout tests must run on the AppKit main thread");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Prohibited);
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            mtm.alloc::<NSWindow>(),
            frame(760.0, 600.0),
            NSWindowStyleMask::Titled | NSWindowStyleMask::Resizable,
            NSBackingStoreType::NSBackingStoreBuffered,
            false,
        )
    };
    let root = ui::view(mtm);
    unsafe {
        root.setFrame(frame(760.0, 600.0));
    }
    window.setContentView(Some(&root));
    let mut page = ScrollStack::new(24.0, mtm);
    ui::attach(&root, &page.scroll);
    for edge in [
        Attribute::Leading,
        Attribute::Trailing,
        Attribute::Top,
        Attribute::Bottom,
    ] {
        ui::constrain(&page.scroll, edge, Some((&root, edge)), 0.0);
    }
    let mut labels = Vec::new();
    for index in 0..20 {
        let body = ui::stack(true, 12.0, mtm);
        let title = page.label(&format!("Application {index}: a long name that must wrap instead of colliding with the next row"), TextStyle::Heading, mtm);
        let detail = page.label("A saved display policy with a long explanation. It must remain readable at narrow and wide logical widths without truncation or overlapping another card.", TextStyle::Body, mtm);
        let socket = page.label(
            &format!(
                "/a/very/long/socket/path/{}/wayland-1",
                "subdirectory/".repeat(12)
            ),
            TextStyle::Mono,
            mtm,
        );
        let heading = ui::stack(false, 10.0, mtm);
        let icon = ui::symbol("display", "Display", mtm);
        ui::arranged(&heading, &icon, false);
        ui::arranged(&heading, &title, false);
        ui::arranged(&body, &heading, true);
        for label in [&detail, &socket] {
            ui::arranged(&body, label, true);
        }
        labels.extend([title, detail, socket]);
        if index == 0 {
            let buttons: Vec<Retained<NSButton>> =
                ["Focus Window", "Copy Connection", "Close Display"]
                    .iter()
                    .map(|title| unsafe {
                        NSButton::buttonWithTitle_target_action(
                            &NSString::from_str(title),
                            None,
                            None,
                            mtm,
                        )
                    })
                    .collect();
            let actions = ResponsiveActions::new(&buttons, mtm);
            ui::arranged(&body, &actions.stack, true);
            page.actions.push(actions);
        }
        let card = ui::card(&body, mtm);
        ui::arranged(&page.body, &card, true);
    }
    let mut heights = Vec::new();
    for width in [760.0, 360.0, 240.0, 1040.0, 360.0] {
        unsafe {
            root.setFrame(frame(width, 600.0));
            root.layoutSubtreeIfNeeded();
        }
        page.reflow();
        unsafe {
            for label in &labels {
                let bounds = label.frame();
                let cell: Retained<objc2::runtime::AnyObject> = msg_send_id![&**label, cell];
                let measured: NSSize =
                    msg_send![&*cell, cellSizeForBounds: frame(bounds.size.width, f64::MAX)];
                assert!(
                    bounds.size.height + 1.0 >= measured.height,
                    "Clipped label at width {width}: actual {:?}, measured {:?}",
                    bounds.size,
                    measured
                );
            }
            let cards = page.body.arrangedSubviews();
            assert_eq!(cards.len(), 20, "Policies past row 16 must remain present");
            for (index, card) in cards.iter().enumerate() {
                for label in &labels[index * 3..index * 3 + 3] {
                    let inside: NSRect =
                        msg_send![&**label, convertRect: label.bounds(), toView: &**card];
                    assert!(
                        inside.origin.y >= 16.0
                            && inside.origin.y + inside.size.height
                                <= card.frame().size.height - 16.0,
                        "Text escaped card {index}: card={:?}, text={inside:?}",
                        card.frame()
                    );
                }
            }
            for (a, b) in cards.iter().zip(cards.iter().skip(1)) {
                assert!(
                    a.frame().origin.y >= b.frame().origin.y + b.frame().size.height + 15.0,
                    "Overlapping cards at width {width}"
                );
            }
            let height = page.body.fittingSize().height;
            heights.push(height);
            println!(
                "Native layout width={width:.0} root={:?} scroll={:?} actions={:?} height={height:.0}: labels fit, 20 cards retained",
                root.frame().size,
                page.scroll.contentSize(),
                page.actions[0].stack.frame().size
            );
            assert!(
                (root.frame().size.width - width).abs() < 1.0,
                "Controls unexpectedly raised the window minimum"
            );
            let expected = if width <= 360.0 {
                NSUserInterfaceLayoutOrientation::Vertical
            } else {
                NSUserInterfaceLayoutOrientation::Horizontal
            };
            assert_eq!(page.actions[0].stack.orientation(), expected);
        }
        page.restore(200.0);
        assert!(
            (page.distance_from_top() - 200.0).abs() < 1.0,
            "Scroll restoration failed at {width}"
        );
    }
    assert!(
        heights[1] > heights[0],
        "Narrowing the window must grow the document"
    );
    assert!(
        (heights[1] - heights[4]).abs() < 1.0,
        "Repeated resize must not accumulate height drift"
    );
    let before = page.distance_from_top();
    unsafe {
        root.setFrame(frame(360.0, 480.0));
        root.layoutSubtreeIfNeeded();
    }
    page.reflow();
    assert!(
        (page.distance_from_top() - before).abs() < 1.0,
        "Height resize moved the top scroll anchor"
    );
    unsafe {
        let _: () = msg_send![&*page.scroll, setScrollerStyle: 0isize];
        page.reflow();
        let document = page.scroll.documentView().unwrap();
        assert!(
            (document.frame().size.width - page.scroll.contentSize().width).abs() < 0.5,
            "Legacy scroller appearance left stale wrapping widths"
        );
        let cards = page.body.arrangedSubviews();
        for card in cards.iter().skip(1) {
            page.body.removeArrangedSubview(card);
            card.removeFromSuperview();
        }
        page.labels.truncate(3);
        page.restore(100_000.0);
        page.reflow();
        let document = page.scroll.documentView().unwrap();
        assert!(
            (document.frame().size.width - page.scroll.contentSize().width).abs() < 0.5,
            "Legacy scroller disappearance left stale wrapping widths"
        );
        assert!(
            document.frame().size.height >= page.scroll.contentSize().height,
            "Short sidebar documents must stay aligned to the top"
        );
        let top = page.body.frame().origin.y + page.body.frame().size.height;
        assert!((document.frame().size.height - top - 24.0).abs() < 1.0);
        assert_eq!(
            page.distance_from_top(),
            0.0,
            "Scroll offset must clamp after rows are removed"
        );
    }
    check_application_toolbar(mtm);
    println!(
        "Native panel layout regression checks passed; no windows displayed or runtimes started."
    );
}

fn check_application_toolbar(mtm: MainThreadMarker) {
    let root = ui::view(mtm);
    let title = ui::single_line("Applications", TextStyle::Title, mtm);
    let actions = ui::stack(false, 8.0, mtm);
    let buttons: Vec<Retained<NSButton>> = ["New Application", "Reload"]
        .iter()
        .map(|title| unsafe {
            NSButton::buttonWithTitle_target_action(&NSString::from_str(title), None, None, mtm)
        })
        .collect();
    for button in &buttons {
        ui::arranged(&actions, button, false);
    }
    let toolbar = ui::toolbar(&title, &actions, mtm);
    ui::attach(&root, &toolbar);
    ui::constrain(
        &toolbar,
        Attribute::Leading,
        Some((&root, Attribute::Leading)),
        20.0,
    );
    ui::constrain(
        &toolbar,
        Attribute::Trailing,
        Some((&root, Attribute::Trailing)),
        -20.0,
    );
    ui::constrain(
        &toolbar,
        Attribute::Top,
        Some((&root, Attribute::Top)),
        14.0,
    );
    ui::constrain(
        &toolbar,
        Attribute::Bottom,
        Some((&root, Attribute::Bottom)),
        -14.0,
    );
    for width in [344.0, 380.0, 470.0, 344.0] {
        unsafe {
            root.setFrame(frame(width, 96.0));
            root.layoutSubtreeIfNeeded();
            assert!((root.frame().size.width - width).abs() < 0.5);
            assert!(
                title.frame().origin.y
                    >= actions.frame().origin.y + actions.frame().size.height + 7.0,
                "Application heading overlaps its controls at width {width}"
            );
            assert!(actions.frame().size.width <= width - 40.0);
            for button in &buttons {
                let bounds: NSRect =
                    msg_send![&**button, convertRect: button.bounds(), toView: &*root];
                assert!(
                    bounds.origin.x >= 18.0 && bounds.origin.x + bounds.size.width <= width - 18.0
                );
                assert!(bounds.origin.y >= 10.0);
            }
        }
    }
    println!(
        "Applications toolbar: separate title/actions fit panes 344, 380, 470 logical pixels."
    );
}
