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
#[path = "../src/render.rs"]
mod render;
