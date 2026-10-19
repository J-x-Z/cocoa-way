//! Isolated tests of the production rendering modules, without AppKit windows.
//! Also buildable directly with rustc --test against Cargo's dependency artifacts
//! while unrelated binary modules are being edited on the shared branch.
#![allow(dead_code)]

#[path = "../src/display_link.rs"]
mod display_link;
#[path = "../src/latency_trace.rs"]
mod latency_trace;
#[path = "../src/layout.rs"]
mod layout;
#[path = "../src/metal_renderer.rs"]
mod metal_renderer;
#[path = "../src/presentation.rs"]
mod presentation;
#[path = "../src/probe_marker.rs"]
mod probe_marker;
#[path = "../src/render.rs"]
mod render;

#[test]
fn commit_trace_captures_arrival_before_marker_decode() {
    // Keep the isolated harness independent of AppState and the in-flight UI edits.
    let state = include_str!("../src/state.rs");
    let (_, commit) = state
        .split_once("fn commit(&mut self, surface: &WlSurface) {")
        .unwrap();
    let (commit, _) = commit.split_once("fn destroyed(").unwrap();
    let captured = commit.find("let commit_arrived_at =").unwrap();
    assert!(
        captured
            < commit
                .find("self.native_cursor.committed(surface)")
                .unwrap()
    );
    assert!(
        captured
            < commit
                .find("crate::render::read_buffer_probe_marker(buffer)")
                .unwrap()
    );
    assert!(commit.contains("self.latency_trace.record_at_ns("));
    assert!(commit.contains("Stage::SurfaceCommit,\n                commit_arrived_at,"));
    assert!(!commit.contains("with_buffer_pixels"));
}

// Source guards only: invoking the live redraw branches requires an AppKit window.
#[test]
fn rootless_redraw_frame_callbacks_flush_immediately_after_enqueue() {
    let source = include_str!("../src/main.rs");
    let (_, rootless) = source
        .split_once("if let presentation::RootlessRenderOutcome::Submitted(rendered) = outcome {")
        .unwrap();
    let (submitted, _) = rootless
        .split_once(
            "} else {\n                            state.rootless_dirty_surfaces.insert(rootless.surface_id());",
        )
        .unwrap();
    let (_, after_callbacks) = submitted
        .split_once("state.send_frame_callbacks_for(Some(&root_surface), callback_time);")
        .unwrap();
    assert_eq!(
        after_callbacks.trim(),
        "flush_wayland_output(&mut display, &wayland_source);",
        "flush this owner's callbacks before leaving the submitted branch",
    );
}

#[test]
fn desktop_redraw_frame_callbacks_flush_for_ready_owners_even_when_another_is_deferred() {
    let source = include_str!("../src/main.rs");
    let (_, desktop) = source.split_once("if deferred && submitted {").unwrap();
    let (deferred, submitted) = desktop.split_once("} else if submitted {").unwrap();
    assert!(deferred.contains("for root in &completed_roots"));
    assert!(deferred.contains("state.send_frame_callbacks_for(Some(root), t);"));
    assert!(deferred.contains("flush_wayland_output(&mut display, &wayland_source);"));
    assert!(!deferred.contains("send_frame_callbacks_for(None"));
    let (submitted, _) = submitted.split_once("} else {").unwrap();
    let (_, after_callbacks) = submitted
        .split_once("state.send_frame_callbacks_for(None, t);")
        .unwrap();
    assert_eq!(
        after_callbacks.trim(),
        "flush_wayland_output(&mut display, &wayland_source);",
        "flush callbacks before leaving the non-deferred submitted branch",
    );
}

#[test]
fn ordinary_input_does_not_drain_control_work_or_refresh_image_clipboard() {
    let source = include_str!("../src/main.rs");
    assert!(source.contains("ControlWorkBudget::new(input_event,"));
    assert!(source.contains("input_policy::requests_clipboard_refresh("));
    let (_, refresh) = source.split_once("if paste_press").unwrap();
    let (refresh, _) = refresh
        .split_once("state.refresh_host_clipboard();")
        .unwrap();
    assert!(refresh.contains("WindowEvent::Focused(true)"));
    assert!(!refresh.contains("WindowEvent::KeyboardInput"));
}

#[test]
fn bounded_wayland_dispatch_reschedules_buffered_work_without_idle_polling() {
    let source = include_str!("../src/main.rs");
    assert!(!source.contains("display.dispatch_clients("));
    assert_eq!(
        source
            .matches("dispatch_wayland_turn(&mut display, &mut state)")
            .count(),
        2
    );
    assert!(source.contains("dispatch_all_clients_with_budget("));
    assert!(source.contains("container_event_signal.finish_turn() || wayland_dispatch_pending"));
    assert!(source.contains("next_wakeup.map_or(ControlFlow::Wait, ControlFlow::WaitUntil)"));
    assert_eq!(source.matches("wayland_source.rearm();").count(), 1);
    let (_, waiting) = source.split_once("Event::AboutToWait => {").unwrap();
    assert!(waiting.contains("wayland_source.rearm();"));
}
