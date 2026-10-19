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
fn desktop_redraw_frame_callbacks_flush_only_in_non_deferred_submitted_branch() {
    let source = include_str!("../src/main.rs");
    let (_, desktop) = source.split_once("if deferred && submitted {").unwrap();
    let (deferred, submitted) = desktop.split_once("} else if submitted {").unwrap();
    assert!(!deferred.contains("send_frame_callbacks_for("));
    assert!(!deferred.contains("flush_wayland_output("));
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
