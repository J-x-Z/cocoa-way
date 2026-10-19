use smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{Client, DisplayHandle, Resource};
use smithay::{
    delegate_compositor, delegate_data_device, delegate_seat, delegate_shm,
    input::{pointer::CursorImageStatus, Seat, SeatHandler, SeatState},
    wayland::{
        buffer::BufferHandler,
        compositor::{CompositorClientState, CompositorHandler, CompositorState},
        selection::data_device::{DataDeviceHandler, WaylandDndGrabHandler},
        selection::SelectionHandler,
        shm::{ShmHandler, ShmState},
    },
};
use smithay::wayland::shell::xdg::{XdgShellHandler, XdgShellState};
use smithay::wayland::shell::xdg::decoration::{XdgDecorationState, XdgDecorationHandler};
use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode as DecorationMode;
use crate::layout::Layout;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use crate::clipboard_transfer::{
    HostClipboardContents, HostImageToken, HostImageWorker, PendingHostImageRequests,
};

#[path = "native_cursor.rs"]
mod native_cursor;

const CLIPBOARD_IMAGE_MIME: &str = "image/png";
const MAX_CLIPBOARD_IMAGE_BYTES: usize = 64 * 1024 * 1024;
const MAX_CLIPBOARD_IMAGE_SOURCE_BYTES: usize = 128 * 1024 * 1024;
const MAX_CLIPBOARD_IMAGE_PIXELS: usize = 32 * 1024 * 1024;
const MAX_SURFACE_ANCESTORS: usize = 128;

fn bounded_parent_chain<T, Id: Eq + std::hash::Hash>(
    first: T,
    id: impl Fn(&T) -> Id,
    mut parent: impl FnMut(&T) -> Option<T>,
) -> Option<Vec<T>> {
    let mut visited = std::collections::HashSet::new();
    let mut chain = Vec::new();
    let mut current = first;
    for _ in 0..MAX_SURFACE_ANCESTORS {
        if !visited.insert(id(&current)) {
            return None;
        }
        let next = parent(&current);
        chain.push(current);
        let Some(next) = next else {
            return Some(chain);
        };
        current = next;
    }
    None
}

type PointerOriginPart = (
    smithay::reexports::wayland_server::backend::ObjectId,
    (i32, i32),
    Option<(i32, i32)>,
    Option<(i32, i32)>,
);

struct HostPasteboardSnapshot {
    change_count: isize,
    text: Option<Arc<str>>,
    image_png: Option<Arc<[u8]>>,
    image_tiff: Option<Box<[u8]>>,
    image_error: Option<String>,
}

enum CopiedPasteboardImage {
    Png(Arc<[u8]>),
    Tiff(Box<[u8]>),
}

pub enum ClipboardPayload {
    Text(Arc<str>),
    Image(Arc<[u8]>),
}

impl ClipboardPayload {
    fn as_bytes(&self) -> &[u8] {
        match self {
            Self::Text(text) => text.as_bytes(),
            Self::Image(image) => image,
        }
    }
}

pub struct PendingFrameCallback {
    pub root_surface: Option<smithay::reexports::wayland_server::backend::ObjectId>,
    pub source_surface: WlSurface,
    pub callback: smithay::reexports::wayland_server::protocol::wl_callback::WlCallback,
    pub trace_commit: Option<u64>,
}

pub struct AppState {
    display_handle: DisplayHandle,
    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    pub shm_state: ShmState,
    pub seat_state: SeatState<AppState>,
    pub seat: Seat<Self>,
    pub data_device_state: smithay::wayland::selection::data_device::DataDeviceState,
    pub data_control_state: smithay::wayland::selection::wlr_data_control::DataControlState,
    _xdg_decoration_state: XdgDecorationState,
    _viewporter_state: smithay::wayland::viewporter::ViewporterState,
    _fractional_scale_state: smithay::wayland::fractional_scale::FractionalScaleManagerState,
    _pointer_constraints_state: smithay::wayland::pointer_constraints::PointerConstraintsState,
    _pointer_gestures_state: smithay::wayland::pointer_gestures::PointerGesturesState,
    _relative_pointer_state: smithay::wayland::relative_pointer::RelativePointerManagerState,
    _cursor_shape_state: smithay::wayland::cursor_shape::CursorShapeManagerState,
    native_cursor: native_cursor::NativeCursor,
    pub pointer_input: crate::pointer_input::PointerInput<winit::window::WindowId, WlSurface>,
    pointer_origin_stamp: Vec<PointerOriginPart>,
    pointer_surface_buffers: std::collections::HashMap<
        smithay::reexports::wayland_server::backend::ObjectId,
        (i32, i32),
    >,
    _output_state: smithay::wayland::output::OutputManagerState,
    pub output: smithay::output::Output,
    pub toplevels: Vec<smithay::wayland::shell::xdg::ToplevelSurface>,
    pub popups: Vec<smithay::wayland::shell::xdg::PopupSurface>,
    pub layout: Layout,
    pub surface_positions: std::collections::HashMap<
        smithay::reexports::wayland_server::backend::ObjectId,
        (i32, i32),
    >,
    pub drag_state: Option<(
        smithay::reexports::wayland_server::backend::ObjectId,
        (f64, f64),
    )>,
    pub start_drag_request: Option<smithay::reexports::wayland_server::backend::ObjectId>,
    pub loop_signal: crate::event_loop_signal::Sender<crate::messages::CompositorMessage>,
    pub presentation: crate::presentation::PresentationMode,
    pub width: u32,
    pub height: u32,
    pub scale_factor: f64,
    /// Monotonic start time — used to compute frame timestamps for wl_callback::done.
    pub start_time: std::time::Instant,
    /// Frame callbacks collected during commit(); fired after swap_buffers().
    pub pending_frame_callbacks: Vec<PendingFrameCallback>,
    /// Native cursor callbacks are paced by the main loop, independently of
    /// scene redraws. Never acknowledge directly from cursor commits.
    pub cursor_frame_callbacks:
        Vec<smithay::reexports::wayland_server::protocol::wl_callback::WlCallback>,
    /// Set by Wayland commits or layout changes so the winit loop can avoid
    /// continuous redraws when the scene is idle.
    pub needs_redraw: bool,
    /// Root toplevels changed since the previous rootless frame. Keeping this
    /// separate avoids redrawing every native window when one application
    /// submits a new buffer.
    pub rootless_dirty_surfaces:
        std::collections::HashSet<smithay::reexports::wayland_server::backend::ObjectId>,
    /// Total Wayland surface commits observed since startup. Used for lightweight
    /// performance diagnostics in Container Mode.
    pub commit_counter: u64,
    pub latency_trace: crate::latency_trace::LatencyTrace,
    pub trace_input: Option<u64>,
    pub trace_forwarded: Option<(u64, u32, u32)>,
    trace_frames:
        std::collections::HashMap<smithay::reexports::wayland_server::backend::ObjectId, u64>,
    host_clipboard: HostClipboardContents,
    host_clipboard_images: HostImageWorker,
    pending_host_image_requests: PendingHostImageRequests,
    pending_guest_clipboard_mime: Option<String>,
    guest_clipboard_generation: Arc<AtomicU64>,
    outgoing_clipboard_transfers: Arc<crate::clipboard_transfer::TransferBudget>,
    pasteboard_change_count: isize,
    last_pasteboard_poll: std::time::Instant,
    pointer_gesture: PointerGestureTracker,
    pointer_axis: PointerAxisTracker,
}

#[derive(Debug)]
struct PointerGestureTracker {
    magnify_active: bool,
    rotation_active: bool,
    protocol_active: bool,
    swipe_active: bool,
    scale: f64,
    rotation: f64,
}

impl Default for PointerGestureTracker {
    fn default() -> Self {
        Self {
            magnify_active: false,
            rotation_active: false,
            protocol_active: false,
            swipe_active: false,
            scale: 1.0,
            rotation: 0.0,
        }
    }
}

#[derive(Debug, Default)]
struct PointerAxisTracker {
    horizontal_active: bool,
    vertical_active: bool,
}

impl PointerAxisTracker {
    fn frame(
        &mut self,
        scale_factor: f64,
        delta: winit::event::MouseScrollDelta,
        phase: winit::event::TouchPhase,
        time: u32,
    ) -> Option<smithay::input::pointer::AxisFrame> {
        use smithay::backend::input::{AxisRelativeDirection, AxisSource};
        use winit::event::{MouseScrollDelta, TouchPhase};

        if phase == TouchPhase::Started {
            self.horizontal_active = false;
            self.vertical_active = false;
        }

        let (axis, source, v120) = match delta {
            MouseScrollDelta::LineDelta(x, y) => {
                let horizontal = -f64::from(x) * 10.0;
                let vertical = -f64::from(y) * 10.0;
                let to_v120 = |value: f32| {
                    (-f64::from(value) * 120.0)
                        .round()
                        .clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32
                };
                (
                    (horizontal, vertical),
                    AxisSource::Wheel,
                    Some((to_v120(x), to_v120(y))),
                )
            }
            MouseScrollDelta::PixelDelta(position) => {
                let logical = position.to_logical::<f64>(scale_factor.max(f64::EPSILON));
                // GTK normalizes 10 Wayland axis units to one step; Firefox's
                // touchpad path expands that step to 40 pixels. AppKit supplies
                // pixel distances, so convert units rather than multiplying speed.
                const AXIS_UNITS_PER_POINT: f64 = 10.0 / 40.0;
                (
                    (
                        -logical.x * AXIS_UNITS_PER_POINT,
                        -logical.y * AXIS_UNITS_PER_POINT,
                    ),
                    AxisSource::Finger,
                    None,
                )
            }
        };

        let horizontal_moved = axis.0 != 0.0;
        let vertical_moved = axis.1 != 0.0;
        // AppKit momentum is filtered by the native monitor. End raw Finger
        // sequences once so the guest can supply its own kinetic scrolling.
        let terminal = matches!(phase, TouchPhase::Ended | TouchPhase::Cancelled);
        let stop = if source == AxisSource::Finger && terminal {
            (
                self.horizontal_active || horizontal_moved,
                self.vertical_active || vertical_moved,
            )
        } else {
            (false, false)
        };

        if source == AxisSource::Finger && !terminal {
            self.horizontal_active |= horizontal_moved;
            self.vertical_active |= vertical_moved;
        }
        if terminal {
            self.horizontal_active = false;
            self.vertical_active = false;
        }

        if !horizontal_moved && !vertical_moved && !stop.0 && !stop.1 {
            return None;
        }

        Some(smithay::input::pointer::AxisFrame {
            source: Some(source),
            time,
            axis,
            stop,
            v120,
            relative_direction: (
                AxisRelativeDirection::Identical,
                AxisRelativeDirection::Identical,
            ),
        })
    }
}

impl AppState {
    pub fn new(
        display_handle: &DisplayHandle,
        scale_factor: f64,
        loop_signal: crate::event_loop_signal::Sender<crate::messages::CompositorMessage>,
        presentation: crate::presentation::PresentationMode,
        width: u32,
        height: u32,
    ) -> Result<Self, String> {
        let compositor_state = CompositorState::new::<Self>(display_handle);
        let xdg_shell_state = XdgShellState::new::<Self>(display_handle);
        let shm_state = ShmState::new::<Self>(
            display_handle,
            vec![
                smithay::reexports::wayland_server::protocol::wl_shm::Format::Argb8888,
                smithay::reexports::wayland_server::protocol::wl_shm::Format::Xrgb8888,
                smithay::reexports::wayland_server::protocol::wl_shm::Format::Abgr8888,
                smithay::reexports::wayland_server::protocol::wl_shm::Format::Xbgr8888,
                smithay::reexports::wayland_server::protocol::wl_shm::Format::Rgba8888,
                smithay::reexports::wayland_server::protocol::wl_shm::Format::Rgbx8888,
                smithay::reexports::wayland_server::protocol::wl_shm::Format::Bgra8888,
                smithay::reexports::wayland_server::protocol::wl_shm::Format::Bgrx8888,
            ],
        );
        let mut seat_state = SeatState::new();
        let mut seat = seat_state.new_wl_seat(display_handle, "winit-seat");
        let xkb_config = smithay::input::keyboard::XkbConfig {
            rules: "evdev",
            model: "pc105",
            layout: "us",
            variant: "",
            options: None,
        };
        seat.add_keyboard(xkb_config, 600, 50).map_err(|error| {
            format!(
                "failed to initialize the Wayland keyboard keymap: {error:?}. Check XKB_CONFIG_ROOT or reinstall Cocoa-Way"
            )
        })?;
        seat.add_pointer();
        let output_state = smithay::wayland::output::OutputManagerState::new_with_xdg_output::<Self>(
            display_handle,
        );
        let output = smithay::output::Output::new(
            "winit".to_string(),
            smithay::output::PhysicalProperties {
                size: (0, 0).into(),
                subpixel: smithay::output::Subpixel::Unknown,
                make: "Smithay".into(),
                model: "Winit".into(),
                serial_number: "0000".into(),
            },
        );
        let _global = output.create_global::<Self>(display_handle);
        let mode = smithay::output::Mode {
            size: (1920, 1080).into(),
            refresh: 60_000,
        };
        let scale_int = scale_factor.round() as i32;
        output.change_current_state(
            Some(mode),
            Some(smithay::utils::Transform::Normal),
            Some(smithay::output::Scale::Integer(scale_int)),
            Some((0, 0).into()),
        );
        output.set_preferred(mode);
        Ok(Self {
            display_handle: display_handle.clone(),
            compositor_state,
            xdg_shell_state,
            shm_state,
            seat_state,
            seat,
            data_device_state: smithay::wayland::selection::data_device::DataDeviceState::new::<Self>(
                display_handle,
            ),
            data_control_state:
                smithay::wayland::selection::wlr_data_control::DataControlState::new::<Self, _>(
                    display_handle,
                    None,
                    |_| true,
                ),
            _xdg_decoration_state: XdgDecorationState::new::<Self>(display_handle),
            _viewporter_state: smithay::wayland::viewporter::ViewporterState::new::<Self>(
                display_handle,
            ),
            _fractional_scale_state:
                smithay::wayland::fractional_scale::FractionalScaleManagerState::new::<Self>(
                    display_handle,
                ),
            _pointer_constraints_state:
                smithay::wayland::pointer_constraints::PointerConstraintsState::new::<Self>(
                    display_handle,
                ),
            _pointer_gestures_state: smithay::wayland::pointer_gestures::PointerGesturesState::new::<
                Self,
            >(display_handle),
            _relative_pointer_state:
                smithay::wayland::relative_pointer::RelativePointerManagerState::new::<Self>(
                    display_handle,
                ),
            _cursor_shape_state: smithay::wayland::cursor_shape::CursorShapeManagerState::new::<Self>(
                display_handle,
            ),
            native_cursor: native_cursor::NativeCursor::default(),
            pointer_input: crate::pointer_input::PointerInput::default(),
            pointer_origin_stamp: Vec::new(),
            pointer_surface_buffers: std::collections::HashMap::new(),
            _output_state: output_state,
            output,
            toplevels: Vec::new(),
            popups: Vec::new(),
            layout: {
                let (logical_width, logical_height) =
                    crate::layout::logical_size_from_physical(width, height, scale_factor);
                Layout::new(logical_width, logical_height)
            },
            surface_positions: std::collections::HashMap::new(),
            drag_state: None,
            start_drag_request: None,
            loop_signal,
            presentation,
            width,
            height,
            scale_factor,
            start_time: std::time::Instant::now(),
            pending_frame_callbacks: Vec::new(),
            cursor_frame_callbacks: Vec::new(),
            needs_redraw: true,
            rootless_dirty_surfaces: std::collections::HashSet::new(),
            latency_trace: crate::latency_trace::LatencyTrace::default(),
            trace_input: None,
            trace_forwarded: None,
            trace_frames: std::collections::HashMap::new(),
            commit_counter: 0,
            host_clipboard: HostClipboardContents::default(),
            host_clipboard_images: HostImageWorker::default(),
            pending_host_image_requests: PendingHostImageRequests::default(),
            pending_guest_clipboard_mime: None,
            guest_clipboard_generation: Arc::new(AtomicU64::new(0)),
            outgoing_clipboard_transfers: Arc::new(
                crate::clipboard_transfer::TransferBudget::default(),
            ),
            pasteboard_change_count: -1,
            last_pasteboard_poll: std::time::Instant::now() - std::time::Duration::from_millis(100),
            pointer_gesture: PointerGestureTracker::default(),
            pointer_axis: PointerAxisTracker::default(),
        })
    }

    /// Call on content CursorEntered, after making winit's cursor visible once.
    pub fn native_cursor_entered(&mut self, window: &winit::window::Window) {
        self.native_cursor.entered(window);
        self.refresh_native_cursor();
    }

    /// Reacquire after focus loss without CursorEntered. Pass WindowEvent's ID
    /// to keep the unchanged-owner motion path free of AppKit/winit FFI calls.
    pub fn native_cursor_motion(
        &mut self,
        window: &winit::window::Window,
        window_id: winit::window::WindowId,
    ) {
        if self.native_cursor.ensure_entered(window, window_id) {
            self.refresh_native_cursor();
        }
    }

    /// Call before pointer.motion(None), and on focus loss/window removal.
    pub fn native_cursor_left(&mut self) {
        self.native_cursor.left();
    }

    /// Window-event cleanup that cannot release a different rootless owner.
    pub fn native_cursor_left_window(&mut self, window_id: winit::window::WindowId) {
        self.native_cursor.left_window(window_id);
    }

    pub fn native_cursor_diagnostics(&self) -> serde_json::Value {
        let mut diagnostics = self.native_cursor.diagnostics();
        diagnostics["pointer_lock"] = serde_json::json!({
            "status": format!("{:?}", self.pointer_input.status()),
            "native_grab": self.pointer_input.native_grab().is_some(),
            "unsupported_confinement_requests": self.pointer_input.unsupported_confinement_requests(),
            "confinement_supported": false,
            "raw_hardware_verified": false,
            "relative_source": "winit_nsevent_not_verified_raw_hardware",
        });
        diagnostics
    }

    pub fn pointer_lock_region(
        &self,
        surface: &WlSurface,
        local: smithay::utils::Point<f64, smithay::utils::Logical>,
        region: Option<&smithay::wayland::compositor::RegionAttributes>,
    ) -> (bool, bool) {
        if !surface.is_alive() {
            return (false, false);
        }
        let Some((buffer_width, buffer_height)) = self.pointer_surface_buffers.get(&surface.id())
        else {
            return (false, false);
        };
        let mapped_root = self
            .root_toplevel_id_for_surface(surface)
            .is_some_and(|root| self.pointer_surface_buffers.contains_key(&root));
        if !mapped_root {
            return (false, false);
        }
        let inside = smithay::wayland::compositor::with_states(surface, |states| {
            use smithay::reexports::wayland_server::protocol::wl_output::Transform;
            use smithay::wayland::compositor::SurfaceAttributes;
            let mut attributes = states.cached_state.get::<SurfaceAttributes>();
            let current = attributes.current();
            let destination = states
                .cached_state
                .get::<smithay::wayland::viewporter::ViewportCachedState>()
                .current()
                .size();
            let (width, height) = match current.buffer_transform {
                Transform::_90 | Transform::_270 | Transform::Flipped90 | Transform::Flipped270 => {
                    (*buffer_height, *buffer_width)
                }
                _ => (*buffer_width, *buffer_height),
            };
            let scale = f64::from(current.buffer_scale.max(1));
            let width = destination.map_or(f64::from(width) / scale, |size| f64::from(size.w));
            let height = destination.map_or(f64::from(height) / scale, |size| f64::from(size.h));
            let point = (local.x.floor() as i32, local.y.floor() as i32);
            crate::pointer_input::inside_surface((local.x, local.y), (width, height))
                && current
                    .input_region
                    .as_ref()
                    .is_none_or(|input| input.contains(point))
                && region.is_none_or(|region| region.contains(point))
        });
        (true, inside)
    }

    pub fn record_pointer_motion(
        &mut self,
        owner: winit::window::WindowId,
        candidate: Option<(
            WlSurface,
            smithay::utils::Point<f64, smithay::utils::Logical>,
        )>,
        actual: Option<WlSurface>,
    ) {
        let grab_focus = self
            .seat
            .get_pointer()
            .and_then(|pointer| pointer.grab_start_data())
            .and_then(|grab| grab.focus)
            .filter(|(surface, _)| actual.as_ref() == Some(surface));
        let (owner, candidate) = if let Some(grab_focus) = grab_focus {
            let Some(known) = self
                .pointer_input
                .focus()
                .filter(|known| actual.as_ref() == Some(&known.surface))
            else {
                self.pointer_input.clear_focus();
                self.pointer_origin_stamp.clear();
                return;
            };
            (known.owner, Some(grab_focus))
        } else {
            (owner, candidate)
        };
        let captured = candidate
            .as_ref()
            .is_some_and(|(surface, _)| actual.as_ref() == Some(surface));
        self.pointer_input.record_motion(
            owner,
            candidate.map(|(surface, origin)| (surface, (origin.x, origin.y))),
            actual.as_ref(),
        );
        if captured {
            self.pointer_origin_stamp = self.pointer_origin_parts(actual.as_ref().unwrap());
        } else if self.pointer_input.focus().is_none() {
            self.pointer_origin_stamp.clear();
        }
    }

    pub fn pointer_focus_origin_current(&self) -> bool {
        self.pointer_input.focus().is_some_and(|focus| {
            focus.surface.is_alive()
                && !self.pointer_origin_stamp.is_empty()
                && self.pointer_origin_stamp == self.pointer_origin_parts(&focus.surface)
        })
    }

    fn pointer_origin_parts(&self, surface: &WlSurface) -> Vec<PointerOriginPart> {
        let mut parts = Vec::new();
        let Some(chain) = self.surface_ancestor_chain(surface) else {
            return parts;
        };
        for current in chain {
            let (offset, geometry) =
                smithay::wayland::compositor::with_states(&current, |states| {
                    let offset = states
                        .cached_state
                        .get::<smithay::wayland::compositor::SubsurfaceCachedState>()
                        .current()
                        .location;
                    let geometry = states
                        .cached_state
                        .get::<smithay::wayland::shell::xdg::SurfaceCachedState>()
                        .current()
                        .geometry
                        .map(|geometry| (geometry.loc.x, geometry.loc.y));
                    ((offset.x, offset.y), geometry)
                });
            let popup = self
                .popups
                .iter()
                .find(|popup| popup.wl_surface() == &current);
            let popup_origin = popup.and_then(|popup| {
                popup.with_committed_state(|state| {
                    state.map(|state| (state.geometry.loc.x, state.geometry.loc.y))
                })
            });
            parts.push((current.id(), offset, geometry, popup_origin));
        }
        parts
    }

    /// Cheap when unchanged; call after pointer motion (before dispatch) to
    /// validate the request owner, and after dispatch to catch lock destruction.
    /// Do not call inside with_pointer_constraint (its mutex would be reentered).
    pub fn refresh_native_cursor(&mut self) {
        let pointer = self.seat.get_pointer();
        let focus = pointer.as_ref().and_then(|pointer| pointer.current_focus());
        let locked = match (&pointer, &focus) {
            (Some(pointer), Some(surface)) => {
                smithay::wayland::pointer_constraints::with_pointer_constraint::<Self, _, _>(
                    surface,
                    pointer,
                    |constraint| {
                        constraint.is_some_and(|constraint| {
                            matches!(
                                *constraint,
                                smithay::wayland::pointer_constraints::PointerConstraint::Locked(_)
                            ) && constraint.is_active()
                        })
                    },
                )
            }
            _ => false,
        };
        self.native_cursor
            .set_focus(focus.map(|surface| surface.id()));
        self.native_cursor.set_locked(locked);
        self.native_cursor.refresh();
    }

    /// Drain only when the main loop's cursor frame clock reaches its deadline.
    pub fn take_cursor_frame_callbacks(
        &mut self,
    ) -> Vec<smithay::reexports::wayland_server::protocol::wl_callback::WlCallback> {
        std::mem::take(&mut self.cursor_frame_callbacks)
    }

    /// Wayland logical units to AppKit points (normally 1; Desktop's 1x mode
    /// on a 2x display uses state.scale_factor / window.scale_factor()).
    pub fn set_native_cursor_point_scale(&mut self, points_per_logical: f64) {
        self.native_cursor.set_point_scale(points_per_logical);
        self.refresh_native_cursor();
    }

    pub fn handle_pointer_axis(
        &mut self,
        scale_factor: f64,
        delta: winit::event::MouseScrollDelta,
        phase: winit::event::TouchPhase,
        time: u32,
    ) {
        let Some(frame) = self.pointer_axis.frame(scale_factor, delta, phase, time) else {
            return;
        };
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        pointer.axis(self, frame);
        pointer.frame(self);
    }

    pub fn handle_pinch_gesture(&mut self, delta: f64, phase: winit::event::TouchPhase, time: u32) {
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };

        match phase {
            winit::event::TouchPhase::Started => {
                self.pointer_gesture.magnify_active = true;
                if !self.pointer_gesture.protocol_active {
                    self.pointer_gesture.protocol_active = true;
                    self.pointer_gesture.scale = 1.0;
                    self.pointer_gesture.rotation = 0.0;
                    pointer.gesture_pinch_begin(
                        self,
                        &smithay::input::pointer::GesturePinchBeginEvent {
                            serial: smithay::utils::SERIAL_COUNTER.next_serial(),
                            time,
                            fingers: 2,
                        },
                    );
                }
            }
            winit::event::TouchPhase::Moved => {
                if !self.pointer_gesture.magnify_active {
                    self.handle_pinch_gesture(0.0, winit::event::TouchPhase::Started, time);
                }
                if delta.is_finite() {
                    let factor = (1.0 + delta).clamp(0.01, 100.0);
                    self.pointer_gesture.scale =
                        (self.pointer_gesture.scale * factor).clamp(0.01, 100.0);
                    pointer.gesture_pinch_update(
                        self,
                        &smithay::input::pointer::GesturePinchUpdateEvent {
                            time,
                            delta: (0.0, 0.0).into(),
                            scale: self.pointer_gesture.scale,
                            rotation: self.pointer_gesture.rotation,
                        },
                    );
                }
            }
            winit::event::TouchPhase::Ended => {
                self.pointer_gesture.magnify_active = false;
                self.finish_pointer_gesture_if_idle(&pointer, time, false);
            }
            winit::event::TouchPhase::Cancelled => {
                self.cancel_pointer_gesture(&pointer, time);
            }
        }
    }

    pub fn handle_swipe_gesture(
        &mut self,
        delta: (f64, f64),
        phase: winit::event::TouchPhase,
        time: u32,
    ) {
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };

        match phase {
            winit::event::TouchPhase::Started => {
                if self.pointer_gesture.swipe_active {
                    pointer.gesture_swipe_end(
                        self,
                        &smithay::input::pointer::GestureSwipeEndEvent {
                            serial: smithay::utils::SERIAL_COUNTER.next_serial(),
                            time,
                            cancelled: true,
                        },
                    );
                }
                self.pointer_gesture.swipe_active = true;
                pointer.gesture_swipe_begin(
                    self,
                    &smithay::input::pointer::GestureSwipeBeginEvent {
                        serial: smithay::utils::SERIAL_COUNTER.next_serial(),
                        time,
                        fingers: 3,
                    },
                );
            }
            winit::event::TouchPhase::Moved => {
                if !self.pointer_gesture.swipe_active {
                    self.handle_swipe_gesture((0.0, 0.0), winit::event::TouchPhase::Started, time);
                }
                if delta.0.is_finite() && delta.1.is_finite() && (delta.0 != 0.0 || delta.1 != 0.0)
                {
                    pointer.gesture_swipe_update(
                        self,
                        &smithay::input::pointer::GestureSwipeUpdateEvent {
                            time,
                            delta: delta.into(),
                        },
                    );
                }
            }
            winit::event::TouchPhase::Ended | winit::event::TouchPhase::Cancelled => {
                if self.pointer_gesture.swipe_active {
                    pointer.gesture_swipe_end(
                        self,
                        &smithay::input::pointer::GestureSwipeEndEvent {
                            serial: smithay::utils::SERIAL_COUNTER.next_serial(),
                            time,
                            cancelled: phase == winit::event::TouchPhase::Cancelled,
                        },
                    );
                    self.pointer_gesture.swipe_active = false;
                }
            }
        }
    }

    pub fn handle_rotation_gesture(
        &mut self,
        delta: f32,
        phase: winit::event::TouchPhase,
        time: u32,
    ) {
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };

        match phase {
            winit::event::TouchPhase::Started => {
                self.pointer_gesture.rotation_active = true;
                if !self.pointer_gesture.protocol_active {
                    self.pointer_gesture.protocol_active = true;
                    self.pointer_gesture.scale = 1.0;
                    self.pointer_gesture.rotation = 0.0;
                    pointer.gesture_pinch_begin(
                        self,
                        &smithay::input::pointer::GesturePinchBeginEvent {
                            serial: smithay::utils::SERIAL_COUNTER.next_serial(),
                            time,
                            fingers: 2,
                        },
                    );
                }
            }
            winit::event::TouchPhase::Moved => {
                if !self.pointer_gesture.rotation_active {
                    self.handle_rotation_gesture(0.0, winit::event::TouchPhase::Started, time);
                }
                if delta.is_finite() {
                    // Winit reports per-event counterclockwise deltas, while Wayland
                    // expects a clockwise rotation relative to the gesture start.
                    self.pointer_gesture.rotation -= f64::from(delta);
                    pointer.gesture_pinch_update(
                        self,
                        &smithay::input::pointer::GesturePinchUpdateEvent {
                            time,
                            delta: (0.0, 0.0).into(),
                            scale: self.pointer_gesture.scale,
                            rotation: self.pointer_gesture.rotation,
                        },
                    );
                }
            }
            winit::event::TouchPhase::Ended => {
                self.pointer_gesture.rotation_active = false;
                self.finish_pointer_gesture_if_idle(&pointer, time, false);
            }
            winit::event::TouchPhase::Cancelled => {
                self.cancel_pointer_gesture(&pointer, time);
            }
        }
    }

    fn finish_pointer_gesture_if_idle(
        &mut self,
        pointer: &smithay::input::pointer::PointerHandle<Self>,
        time: u32,
        cancelled: bool,
    ) {
        if self.pointer_gesture.protocol_active
            && !self.pointer_gesture.magnify_active
            && !self.pointer_gesture.rotation_active
        {
            pointer.gesture_pinch_end(
                self,
                &smithay::input::pointer::GesturePinchEndEvent {
                    serial: smithay::utils::SERIAL_COUNTER.next_serial(),
                    time,
                    cancelled,
                },
            );
            self.pointer_gesture.protocol_active = false;
            self.pointer_gesture.scale = 1.0;
            self.pointer_gesture.rotation = 0.0;
        }
    }

    fn cancel_pointer_gesture(
        &mut self,
        pointer: &smithay::input::pointer::PointerHandle<Self>,
        time: u32,
    ) {
        self.pointer_gesture.magnify_active = false;
        self.pointer_gesture.rotation_active = false;
        self.finish_pointer_gesture_if_idle(pointer, time, true);
    }

    fn surface_ancestor_chain(&self, surface: &WlSurface) -> Option<Vec<WlSurface>> {
        bounded_parent_chain(surface.clone(), Resource::id, |current| {
            smithay::wayland::compositor::get_parent(current).or_else(|| {
                self.popups
                    .iter()
                    .find(|popup| popup.wl_surface() == current)
                    .and_then(|popup| popup.get_parent_surface())
            })
        })
    }

    fn root_toplevel_id_for_surface(
        &self,
        surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
    ) -> Option<smithay::reexports::wayland_server::backend::ObjectId> {
        let chain = self.surface_ancestor_chain(surface)?;
        let root = chain.last()?;
        self.toplevels
            .iter()
            .find(|toplevel| toplevel.wl_surface() == root)
            .map(|toplevel| toplevel.wl_surface().id())
    }

    /// Unmap a role's scene without destroying its reusable wl_surface.
    fn remove_surface_from_scene(&mut self, surface: &WlSurface, was_toplevel: bool) {
        use smithay::wayland::compositor::{TraversalAction, with_surface_tree_downward};

        let surface_id = surface.id();
        let affected_root = self.root_toplevel_id_for_surface(surface);
        // Resolve popup ancestry before removing the role that connects the tree.
        let mut scene_roots = vec![surface.clone()];
        scene_roots.extend(self.popups.iter().filter_map(|popup| {
            (popup.wl_surface() != surface
                && self
                    .surface_ancestor_chain(popup.wl_surface())
                    .is_some_and(|chain| chain.iter().any(|ancestor| ancestor == surface)))
            .then(|| popup.wl_surface().clone())
        }));
        let mut retired = std::collections::HashSet::new();
        for root in scene_roots {
            with_surface_tree_downward(
                &root,
                (),
                |_, _, _| TraversalAction::DoChildren(()),
                |child, _, _| {
                    retired.insert(child.id());
                    self.pointer_input.forget_surface(child);
                },
                |_, _, _| true,
            );
        }
        self.pointer_surface_buffers
            .retain(|id, _| !retired.contains(id));
        self.surface_positions.retain(|id, _| !retired.contains(id));
        self.layout.remove_tile(&surface_id);
        self.toplevels
            .retain(|toplevel| toplevel.wl_surface() != surface);
        for popup in &self.popups {
            if popup.wl_surface() != surface
                && retired.contains(&popup.wl_surface().id())
                && popup.alive()
            {
                popup.send_popup_done();
            }
        }
        self.popups
            .retain(|popup| !retired.contains(&popup.wl_surface().id()));
        self.pending_frame_callbacks.retain(|pending| {
            !retired.contains(&pending.source_surface.id())
                && !(was_toplevel && pending.root_surface.as_ref() == Some(&surface_id))
        });
        if was_toplevel {
            self.output.leave(surface);
            self.rootless_dirty_surfaces.remove(&surface_id);
            self.trace_frames.remove(&surface_id);
        }
        if self
            .drag_state
            .as_ref()
            .is_some_and(|(id, _)| retired.contains(id))
        {
            self.drag_state = None;
        }
        if self
            .start_drag_request
            .as_ref()
            .is_some_and(|id| retired.contains(id))
        {
            self.start_drag_request = None;
        }
        if self.pointer_input.focus().is_none() {
            self.pointer_origin_stamp.clear();
        }
        if let Some(pointer) = self.seat.get_pointer()
            && pointer
                .current_focus()
                .is_some_and(|focus| retired.contains(&focus.id()))
        {
            self.native_cursor_left();
            let time = self.start_time.elapsed().as_millis() as u32;
            self.cancel_pointer_gesture(&pointer, time);
            crate::clear_pointer_focus(self, time);
        }
        self.refresh_native_cursor();
        if let Some(keyboard) = self.seat.get_keyboard()
            && keyboard
                .current_focus()
                .is_some_and(|focus| retired.contains(&focus.id()))
        {
            keyboard.set_focus(self, None, smithay::utils::SERIAL_COUNTER.next_serial());
        }
        if self.presentation.is_rootless() {
            if !was_toplevel && let Some(root) = affected_root {
                self.rootless_dirty_surfaces.insert(root);
            }
            self.needs_redraw = !self.rootless_dirty_surfaces.is_empty();
            if was_toplevel {
                let _ = self.loop_signal.send(
                    crate::messages::CompositorMessage::RootlessToplevelDestroyed(surface_id),
                );
            }
        } else {
            self.needs_redraw = true;
        }
    }

    pub fn trace_frames_for(
        &self,
        root: Option<&smithay::reexports::wayland_server::backend::ObjectId>,
    ) -> Vec<u64> {
        if let Some(root) = root {
            self.trace_frames.get(root).copied().into_iter().collect()
        } else {
            self.trace_frames.values().copied().collect()
        }
    }

    pub fn retire_trace_frames(&mut self, submitted: &[u64]) {
        self.trace_frames.retain(|_, id| !submitted.contains(id));
    }

    fn take_frame_callbacks_for(
        &mut self,
        root_surface: Option<&smithay::reexports::wayland_server::backend::ObjectId>,
    ) -> Vec<PendingFrameCallback> {
        if root_surface.is_none() {
            return std::mem::take(&mut self.pending_frame_callbacks);
        }

        let mut selected = Vec::new();
        let mut remaining = Vec::new();
        for pending in std::mem::take(&mut self.pending_frame_callbacks) {
            let resolved_root = pending.root_surface.clone().or_else(|| {
                pending
                    .source_surface
                    .is_alive()
                    .then(|| self.root_toplevel_id_for_surface(&pending.source_surface))
                    .flatten()
            });
            if resolved_root.as_ref() == root_surface {
                selected.push(pending);
            } else {
                remaining.push(pending);
            }
        }
        self.pending_frame_callbacks = remaining;
        selected
    }

    pub fn send_frame_callbacks_for(
        &mut self,
        root_surface: Option<&smithay::reexports::wayland_server::backend::ObjectId>,
        time: u32,
    ) {
        let mut notified = std::collections::HashSet::new();
        for pending in self.take_frame_callbacks_for(root_surface) {
            pending.callback.done(time);
            if let Some(id) = pending.trace_commit
                && notified.insert(id)
            {
                self.latency_trace.record(
                    crate::latency_trace::Stage::FrameCallbackDone,
                    crate::latency_trace::Tags {
                        correlation_id: Some(crate::latency_trace::Tag::U64(id)),
                        ..Default::default()
                    },
                );
            }
        }
    }

    pub fn poll_host_clipboard(&mut self) {
        self.poll_host_clipboard_results();
        let now = std::time::Instant::now();
        if now.duration_since(self.last_pasteboard_poll) < std::time::Duration::from_millis(100) {
            return;
        }
        self.refresh_host_clipboard();
    }

    /// Focus and paste input must not wait for the background maintenance timer.
    pub fn refresh_host_clipboard(&mut self) {
        self.last_pasteboard_poll = std::time::Instant::now();

        let Some(snapshot) = pasteboard_snapshot_if_changed(self.pasteboard_change_count) else {
            self.poll_host_clipboard_results();
            return;
        };
        let generation = self
            .guest_clipboard_generation
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1);
        self.pending_guest_clipboard_mime = None;
        self.pasteboard_change_count = snapshot.change_count;
        self.host_clipboard = HostClipboardContents::from_snapshot(
            snapshot.text,
            snapshot.image_png,
            snapshot.image_tiff.is_some() || snapshot.image_error.is_some(),
        );
        let token = HostImageToken {
            generation,
            change_count: snapshot.change_count,
        };
        self.pending_host_image_requests
            .reset(snapshot.image_tiff.as_ref().map(|_| token));
        self.host_clipboard_images
            .replace_pending(snapshot.image_tiff.map(|tiff| (token, tiff)));

        self.publish_host_clipboard();
        if let Some(error) = snapshot.image_error {
            log::warn!("Clipboard: {error}");
            crate::diagnostics::record_clipboard_host_failure(error);
        }
        self.poll_host_clipboard_results();
    }

    /// Parent supplies an EventLoopProxy waker before the first clipboard refresh.
    pub fn set_host_clipboard_waker(&mut self, wake: Arc<dyn Fn() + Send + Sync>) {
        self.host_clipboard_images.set_waker(wake);
    }

    /// Include this in the existing maintenance work decision, not a new timer.
    pub fn has_pending_host_clipboard_image(&self) -> bool {
        self.host_clipboard_images.has_work()
    }

    /// Cheap completion polling for maintenance or an existing event wake.
    /// No provider fetching or image conversion takes place here.
    pub fn poll_host_clipboard_results(&mut self) {
        if !self.host_clipboard_images.has_work() {
            return;
        }
        let generation = self.guest_clipboard_generation.load(Ordering::Relaxed);
        let live_count = unsafe { objc2_app_kit::NSPasteboard::generalPasteboard().changeCount() };
        if let Some(token) = self.pending_host_image_requests.token()
            && !token.is_current(generation, self.pasteboard_change_count, live_count)
        {
            self.pending_host_image_requests.reset(None);
        }
        if let Some(completion) = self.host_clipboard_images.poll() {
            let error = completion.result.as_ref().err().cloned();
            let token = completion.token;
            let current = token.is_current(generation, self.pasteboard_change_count, live_count);
            let fds = self.pending_host_image_requests.take(token);
            let has_image = completion
                .result
                .as_ref()
                .is_ok_and(|image| image.is_some());
            let _ = self.host_clipboard.merge_image(
                completion,
                generation,
                self.pasteboard_change_count,
                live_count,
            );
            if current {
                if has_image {
                    // PNG was already offered while pending; do not invalidate an
                    // immediate paste's offer by re-publishing the same MIME list.
                    self.record_host_clipboard_change();
                    if let Some(image) = &self.host_clipboard.image_png {
                        for fd in fds {
                            self.send_host_clipboard_payload(
                                fd,
                                ClipboardPayload::Image(image.clone()),
                            );
                        }
                    }
                } else {
                    self.publish_host_clipboard();
                }
            }
            // Publish first: recording a host change clears diagnostics' errors.
            // Surface failures even when their snapshot has been superseded.
            if let Some(error) = error {
                log::warn!("Clipboard: {error}");
                crate::diagnostics::record_clipboard_host_failure(error);
            }
        }
        if let Some(token) = self.host_clipboard_images.pending_token() {
            if !token.is_current(generation, self.pasteboard_change_count, live_count) {
                self.host_clipboard_images.replace_pending(None);
            } else if let Err(error) = self
                .host_clipboard_images
                .start_pending(convert_host_clipboard_tiff)
            {
                let error = format!("Failed to start macOS clipboard image conversion: {error}");
                drop(self.pending_host_image_requests.take(token));
                self.publish_host_clipboard();
                log::warn!("Clipboard: {error}");
                crate::diagnostics::record_clipboard_host_failure(error);
            }
        }
    }

    fn record_host_clipboard_change(&self) -> Vec<String> {
        let byte_count = self
            .host_clipboard
            .text
            .as_ref()
            .map_or(0, |text| text.len())
            + self
                .host_clipboard
                .image_png
                .as_ref()
                .map_or(0, |image| image.len());
        let mime_types = clipboard_mime_types(
            self.host_clipboard.text.is_some(),
            self.host_clipboard.image_png.is_some()
                || self.pending_host_image_requests.token().is_some(),
        );
        // A pending image has no encoded bytes yet, but is not an empty copy.
        if byte_count != 0 || self.pending_host_image_requests.token().is_none() {
            crate::diagnostics::record_clipboard_host_change(
                byte_count,
                mime_types.first().map(String::as_str),
            );
        }
        mime_types
    }

    fn publish_host_clipboard(&mut self) {
        let mime_types = self.record_host_clipboard_change();

        if mime_types.is_empty() {
            log::info!("Clipboard: macOS pasteboard has no supported contents");
            smithay::wayland::selection::data_device::clear_data_device_selection::<Self>(
                &self.display_handle,
                &self.seat,
            );
            return;
        }

        log::info!(
            "Clipboard: publishing changed macOS contents to Wayland clients as {}",
            mime_types.join(", ")
        );
        smithay::wayland::selection::data_device::set_data_device_selection::<Self>(
            &self.display_handle,
            &self.seat,
            mime_types,
            HostImageToken {
                generation: self.guest_clipboard_generation.load(Ordering::Relaxed),
                change_count: self.pasteboard_change_count,
            },
        );
    }

    pub fn install_guest_clipboard(
        &mut self,
        generation: u64,
        pasteboard_change_count: isize,
        payload: ClipboardPayload,
    ) {
        // A slow source must not replace a newer guest selection or host copy.
        let source = HostImageToken {
            generation,
            change_count: pasteboard_change_count,
        };
        if !source.source_is_current(
            self.guest_clipboard_generation.load(Ordering::Relaxed),
            unsafe { objc2_app_kit::NSPasteboard::generalPasteboard().changeCount() },
        ) {
            return;
        }
        self.host_clipboard_images.replace_pending(None);
        self.pending_host_image_requests.reset(None);
        let byte_count = payload.as_bytes().len();
        match payload {
            ClipboardPayload::Text(text) => {
                if self.host_clipboard.matches_text_only(&text) {
                    return;
                }
                self.pasteboard_change_count = write_to_pasteboard(&text);
                self.host_clipboard = HostClipboardContents::from_snapshot(Some(text), None, false);
            }
            ClipboardPayload::Image(image) => {
                if self.host_clipboard.image_png.as_deref() == Some(&*image)
                    && self.host_clipboard.text.is_none()
                {
                    return;
                }
                use objc2_app_kit::{NSPasteboard, NSPasteboardTypePNG};
                use objc2_foundation::NSData;
                unsafe {
                    let pb = NSPasteboard::generalPasteboard();
                    pb.clearContents();
                    if !pb.setData_forType(Some(&NSData::with_bytes(&image)), NSPasteboardTypePNG) {
                        crate::diagnostics::record_clipboard_failure(
                            "Failed to install Wayland PNG on macOS",
                        );
                        return;
                    }
                    self.pasteboard_change_count = pb.changeCount();
                }
                self.host_clipboard =
                    HostClipboardContents::from_snapshot(None, Some(image), false);
            }
        }
        crate::diagnostics::record_clipboard_guest_install(byte_count);
        log::info!(
            "Clipboard: installed Wayland contents on the macOS pasteboard ({byte_count} bytes)"
        );
        smithay::wayland::selection::data_device::set_data_device_selection::<Self>(
            &self.display_handle,
            &self.seat,
            clipboard_mime_types(
                self.host_clipboard.text.is_some(),
                self.host_clipboard.image_png.is_some(),
            ),
            HostImageToken {
                generation,
                change_count: self.pasteboard_change_count,
            },
        );
    }

    fn send_host_clipboard_payload(&self, fd: std::os::fd::OwnedFd, payload: ClipboardPayload) {
        if let Err(error) =
            self.outgoing_clipboard_transfers
                .spawn(payload.as_bytes().len(), move || {
                    if let Err(error) = write_clipboard_payload(fd, payload.as_bytes()) {
                        crate::diagnostics::record_clipboard_host_failure(format!(
                            "Failed to send macOS clipboard contents to Wayland: {error}"
                        ));
                    }
                })
        {
            crate::diagnostics::record_clipboard_host_failure(format!(
                "Unable to start macOS clipboard transfer: {error}"
            ));
        }
    }

    /// Read a client selection only after Smithay has committed it to the seat.
    /// `SelectionHandler::new_selection` runs before that protocol state update.
    pub fn request_pending_guest_clipboard(&mut self) {
        let Some(mime) = self.pending_guest_clipboard_mime.take() else {
            return;
        };
        let (read_fd, write_fd) = match nix_pipe() {
            Some(pair) => pair,
            None => {
                crate::diagnostics::record_clipboard_failure(
                    "Unable to create a pipe for the Wayland clipboard transfer",
                );
                return;
            }
        };
        if let Err(error) =
            smithay::wayland::selection::data_device::request_data_device_client_selection::<AppState>(
                &self.seat,
                mime.clone(),
                write_fd,
            )
        {
            log::warn!("Failed to request Wayland clipboard contents as {mime}: {error}");
            crate::diagnostics::record_clipboard_failure(format!(
                "Failed to request Wayland clipboard contents as {mime}: {error}"
            ));
            return;
        }

        let loop_signal = self.loop_signal.clone();
        let generation_counter = self.guest_clipboard_generation.clone();
        let generation = generation_counter.load(Ordering::Relaxed);
        let pasteboard_change_count =
            unsafe { objc2_app_kit::NSPasteboard::generalPasteboard().changeCount() };
        if let Err(error) = std::thread::Builder::new()
            .name("clipboard-read".into())
            .spawn(move || {
                let result = read_guest_clipboard(read_fd, &mime, &generation_counter, generation);
                match result {
                    Ok(Some(payload)) => {
                        let _ =
                            loop_signal.send(crate::messages::CompositorMessage::GuestClipboard {
                                generation,
                                pasteboard_change_count,
                                payload,
                            });
                    }
                    Ok(None) => {}
                    Err(error) => crate::diagnostics::record_clipboard_failure(error),
                }
            })
        {
            crate::diagnostics::record_clipboard_failure(format!(
                "Failed to start Wayland clipboard reader: {error}"
            ));
        }
    }
    pub fn update_scale_factor(&mut self, scale: f64) {
        let scale = if scale.is_finite() && scale > 0.0 {
            scale
        } else {
            log::warn!("Ignoring invalid window scale factor: {}", scale);
            1.0
        };
        if self.scale_factor != scale && !self.presentation.is_rootless() {
            self.pointer_input.clear_focus();
        }
        self.scale_factor = scale;
        self.output.change_current_state(
            None,
            None,
            Some(smithay::output::Scale::Integer(
                (scale.round() as i32).clamp(1, 8),
            )),
            None,
        );
        let (logical_width, logical_height) =
            crate::layout::logical_size_from_physical(self.width, self.height, scale);
        self.layout.set_view_size(logical_width, logical_height);
        for tile in &self.layout.tiles {
            tile.request_size();
        }
    }
}
impl smithay::wayland::output::OutputHandler for AppState {}
smithay::delegate_output!(AppState);
delegate_compositor!(AppState);
delegate_shm!(AppState);
delegate_seat!(AppState);
smithay::delegate_xdg_shell!(AppState);
impl CompositorHandler for AppState {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }
    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        let client_data = client
            .get_data::<ClientState>()
            .expect("Client data missing");
        &client_data.compositor_state
    }
    fn new_surface(&mut self, _surface: &WlSurface) {
        // No-op: pre-commit hook logging removed to avoid 60fps log spam
    }
    fn commit(&mut self, surface: &WlSurface) {
        let commit_arrived_at = self
            .latency_trace
            .enabled()
            .then(crate::latency_trace::now_ns);
        if let Some(callbacks) = self.native_cursor.committed(surface) {
            self.cursor_frame_callbacks.extend(callbacks);
            self.commit_counter = self.commit_counter.saturating_add(1);
            return;
        }
        use smithay::wayland::compositor::{
            SurfaceAttributes, TraversalAction, with_surface_tree_downward,
        };
        // Resolve the owner before walking the tree. Smithay holds its surface-tree
        // mutex during traversal, so calling get_parent from the visitor deadlocks.
        let root_surface = self.root_toplevel_id_for_surface(surface);
        let trace_commit = if let Some(commit_arrived_at) = commit_arrived_at {
            let id = crate::latency_trace::next_id();
            let surface_id = format!("{:?}", surface.id());
            let (probe_marker, commit_flags) =
                smithay::wayland::compositor::with_states(surface, |states| {
                    let mut attributes = states.cached_state.get::<SurfaceAttributes>();
                    let current = attributes.current();
                    let flags = u64::from(current.buffer.is_some())
                        | (u64::from(!current.damage.is_empty()) << 1)
                        | (u64::from(!current.frame_callbacks.is_empty()) << 2);
                    let marker = match current.buffer.as_ref() {
                        Some(smithay::wayland::compositor::BufferAssignment::NewBuffer(buffer)) => {
                            crate::render::read_buffer_probe_marker(buffer)
                        }
                        _ => None,
                    };
                    (marker, flags)
                });
            self.latency_trace.record_at_ns(
                crate::latency_trace::Stage::SurfaceCommit,
                commit_arrived_at,
                crate::latency_trace::Tags {
                    correlation_id: Some(crate::latency_trace::Tag::U64(id)),
                    probe_marker,
                    surface_id: Some(crate::latency_trace::Tag::String(&surface_id)),
                    commit_flags: Some(commit_flags),
                    ..Default::default()
                },
            );
            if let Some(root) = root_surface.as_ref() {
                // Coalesced commits stay unmatched; only the latest owner frame
                // is carried through composition, never the latest input.
                self.trace_frames.insert(root.clone(), id);
            }
            Some(id)
        } else {
            None
        };
        let mut new_cbs = Vec::new();
        let mut input_buffers = Vec::new();
        with_surface_tree_downward(
            surface,
            (),
            |_, _, _| TraversalAction::DoChildren(()),
            |callback_surface, states, _| {
                let mut guard = states.cached_state.get::<SurfaceAttributes>();
                match guard.current().buffer.as_ref() {
                    Some(smithay::wayland::compositor::BufferAssignment::NewBuffer(buffer)) => {
                        let size =
                            smithay::wayland::shm::with_buffer_contents(buffer, |_, _, data| {
                                (data.width, data.height)
                            })
                            .ok()
                            .filter(|(width, height)| *width > 0 && *height > 0);
                        input_buffers.push((callback_surface.id(), size));
                    }
                    Some(smithay::wayland::compositor::BufferAssignment::Removed) => {
                        input_buffers.push((callback_surface.id(), None));
                    }
                    None => {}
                }
                new_cbs.extend(guard.current().frame_callbacks.drain(..).map(|callback| {
                    PendingFrameCallback {
                        root_surface: root_surface.clone(),
                        source_surface: callback_surface.clone(),
                        callback,
                        // Child callbacks may originate in a different commit.
                        // Leave them unmatched instead of inventing an association.
                        trace_commit: (callback_surface == surface)
                            .then_some(trace_commit)
                            .flatten(),
                    }
                }));
            },
            |_, _, _| true,
        );
        for (surface, size) in input_buffers {
            if let Some(size) = size {
                self.pointer_surface_buffers.insert(surface, size);
            } else {
                self.pointer_surface_buffers.remove(&surface);
            }
        }
        self.pending_frame_callbacks.extend(new_cbs);
        self.commit_counter = self.commit_counter.saturating_add(1);
        if self.presentation.is_rootless() {
            if let Some(root_surface) = root_surface {
                self.rootless_dirty_surfaces.insert(root_surface);
                self.needs_redraw = true;
            }
        } else {
            self.needs_redraw = true;
        }
    }

    fn destroyed(&mut self, surface: &WlSurface) {
        if self.native_cursor.destroyed(surface) {
            self.pending_frame_callbacks
                .retain(|pending| pending.source_surface != *surface);
            self.refresh_native_cursor();
            return;
        }
        let surface_id = surface.id();
        let was_toplevel = self
            .toplevels
            .iter()
            .any(|toplevel| toplevel.wl_surface() == surface);
        self.remove_surface_from_scene(surface, was_toplevel);
        let _ = self
            .loop_signal
            .send(crate::messages::CompositorMessage::SurfaceDestroyed(
                surface_id.clone(),
            ));
        log::info!("Removed destroyed surface {surface_id:?} from the compositor layout");
    }
}
impl XdgShellHandler for AppState {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }
    fn new_toplevel(&mut self, surface: smithay::wayland::shell::xdg::ToplevelSurface) {
        log::info!("New XDG Toplevel Created: {:?}", surface.wl_surface().id());
        // Smithay's ToplevelSurface equality only compares wl_surface, not the role.
        if !self
            .toplevels
            .iter()
            .any(|known| known.xdg_toplevel() == surface.xdg_toplevel())
        {
            if let Some(previous) = self
                .toplevels
                .iter()
                .find(|known| known.wl_surface() == surface.wl_surface())
                .cloned()
            {
                self.toplevel_destroyed(previous);
            }
            self.toplevels.push(surface.clone());
            self.output.enter(surface.wl_surface());
            if self.presentation.is_rootless() {
                self.rootless_dirty_surfaces
                    .insert(surface.wl_surface().id());
                let _ = self.loop_signal.send(
                    crate::messages::CompositorMessage::RootlessToplevelCreated(
                        surface.wl_surface().id(),
                    ),
                );
            } else {
                self.layout.add_tile(surface.clone());
                let _ = self
                    .loop_signal
                    .send(crate::messages::CompositorMessage::DesktopToplevelCreated);
            }
            self.needs_redraw = true;
        }
        // Tell the client the compositor window size and scale so it renders at
        // the correct HiDPI resolution.
        let (logical_w, logical_h) = if self.presentation.is_rootless() {
            (960, 720)
        } else {
            crate::layout::logical_size_from_physical(self.width, self.height, self.scale_factor)
        };
        surface.with_pending_state(|state| {
            state.states.set(smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State::Activated);
            state.size = Some((logical_w, logical_h).into());
        });
        // Notify the client of the compositor's fractional scale so it can
        // render at the correct resolution without needing integer rounding.
        smithay::wayland::compositor::with_states(surface.wl_surface(), |states| {
            smithay::wayland::fractional_scale::with_fractional_scale(states, |fs| {
                fs.set_preferred_scale(self.scale_factor);
            });
        });
        surface.send_configure();
    }
    fn new_popup(
        &mut self,
        surface: smithay::wayland::shell::xdg::PopupSurface,
        positioner: smithay::wayland::shell::xdg::PositionerState,
    ) {
        if self
            .popups
            .iter()
            .any(|known| known.xdg_popup() == surface.xdg_popup())
        {
            return;
        }
        if let Some(previous) = self
            .popups
            .iter()
            .find(|known| known.wl_surface() == surface.wl_surface())
            .cloned()
        {
            self.popup_destroyed(previous);
        }
        let geo = positioner.get_geometry();
        surface.with_pending_state(|state| {
            state.geometry = geo;
        });
        if surface.send_configure().is_err() {
            return;
        }
        let root = self.root_toplevel_id_for_surface(
            surface
                .get_parent_surface()
                .as_ref()
                .unwrap_or(surface.wl_surface()),
        );
        self.popups.push(surface);
        if self.presentation.is_rootless()
            && let Some(root) = root
        {
            self.rootless_dirty_surfaces.insert(root);
        }
        self.needs_redraw = true;
    }
    fn toplevel_destroyed(&mut self, surface: smithay::wayland::shell::xdg::ToplevelSurface) {
        // A late callback for an old role must not unmap its replacement.
        if self
            .toplevels
            .iter()
            .any(|known| known.xdg_toplevel() == surface.xdg_toplevel())
        {
            self.remove_surface_from_scene(surface.wl_surface(), true);
        }
    }
    fn popup_destroyed(&mut self, surface: smithay::wayland::shell::xdg::PopupSurface) {
        if self
            .popups
            .iter()
            .any(|known| known.xdg_popup() == surface.xdg_popup())
        {
            self.remove_surface_from_scene(surface.wl_surface(), false);
        }
    }
    fn grab(
        &mut self,
        _surface: smithay::wayland::shell::xdg::PopupSurface,
        _seat: smithay::reexports::wayland_server::protocol::wl_seat::WlSeat,
        _serial: smithay::utils::Serial,
    ) {
    }
    fn reposition_request(
        &mut self,
        surface: smithay::wayland::shell::xdg::PopupSurface,
        positioner: smithay::wayland::shell::xdg::PositionerState,
        token: u32,
    ) {
        let geometry = positioner.get_geometry();
        surface.with_pending_state(|state| state.geometry = geometry);
        surface.send_repositioned(token);
        if self.presentation.is_rootless()
            && let Some(root) = self.root_toplevel_id_for_surface(surface.wl_surface())
        {
            self.rootless_dirty_surfaces.insert(root);
        }
        self.needs_redraw = true;
    }
    fn maximize_request(&mut self, surface: smithay::wayland::shell::xdg::ToplevelSurface) {
        log::info!("Maximize Request: {:?}", surface.wl_surface().id());
        if self.presentation.is_rootless() {
            surface.with_pending_state(|state| {
                state.states.set(smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State::Maximized);
            });
            surface.send_configure();
            let _ = self
                .loop_signal
                .send(crate::messages::CompositorMessage::RootlessMaximize {
                    surface: surface.wl_surface().id(),
                    maximized: true,
                });
            return;
        }
        let (logical_w, logical_h) =
            crate::layout::logical_size_from_physical(self.width, self.height, self.scale_factor);
        log::info!(
            "Maximizing to Logical Size: {}x{} (Physical: {}x{}, Scale: {})",
            logical_w,
            logical_h,
            self.width,
            self.height,
            self.scale_factor
        );
        surface.with_pending_state(|state| {
            state.states.set(smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State::Maximized);
            state.size = Some((logical_w, logical_h).into());
        });
        surface.send_configure();
        let _ = self
            .loop_signal
            .send(crate::messages::CompositorMessage::Maximize(true));
    }
    fn unmaximize_request(&mut self, surface: smithay::wayland::shell::xdg::ToplevelSurface) {
        log::info!("Unmaximize Request: {:?}", surface.wl_surface().id());
        surface.with_pending_state(|state| {
             state.states.unset(smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State::Maximized);
         });
        surface.send_configure();
        if self.presentation.is_rootless() {
            let _ = self
                .loop_signal
                .send(crate::messages::CompositorMessage::RootlessMaximize {
                    surface: surface.wl_surface().id(),
                    maximized: false,
                });
            return;
        }
        let _ = self
            .loop_signal
            .send(crate::messages::CompositorMessage::Maximize(false));
    }
    fn fullscreen_request(
        &mut self,
        surface: smithay::wayland::shell::xdg::ToplevelSurface,
        _output: Option<smithay::reexports::wayland_server::protocol::wl_output::WlOutput>,
    ) {
        log::info!("Fullscreen Request: {:?}", surface.wl_surface().id());
        if self.presentation.is_rootless() {
            surface.with_pending_state(|state| {
                state.states.set(smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State::Fullscreen);
            });
            surface.send_configure();
            let _ = self
                .loop_signal
                .send(crate::messages::CompositorMessage::RootlessFullscreen {
                    surface: surface.wl_surface().id(),
                    fullscreen: true,
                });
            return;
        }
        let (logical_w, logical_h) =
            crate::layout::logical_size_from_physical(self.width, self.height, self.scale_factor);
        log::info!("Fullscreening to Logical Size: {}x{}", logical_w, logical_h);
        surface.with_pending_state(|state| {
             state.states.set(smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State::Fullscreen);
             state.size = Some((logical_w, logical_h).into());
         });
        surface.send_configure();
        let _ = self
            .loop_signal
            .send(crate::messages::CompositorMessage::Fullscreen(true));
    }
    fn unfullscreen_request(&mut self, surface: smithay::wayland::shell::xdg::ToplevelSurface) {
        log::info!("Unfullscreen Request: {:?}", surface.wl_surface().id());
        surface.with_pending_state(|state| {
             state.states.unset(smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State::Fullscreen);
        });
        surface.send_configure();
        if self.presentation.is_rootless() {
            let _ = self
                .loop_signal
                .send(crate::messages::CompositorMessage::RootlessFullscreen {
                    surface: surface.wl_surface().id(),
                    fullscreen: false,
                });
            return;
        }
        let _ = self
            .loop_signal
            .send(crate::messages::CompositorMessage::Fullscreen(false));
    }
    fn move_request(
        &mut self,
        surface: smithay::wayland::shell::xdg::ToplevelSurface,
        _seat: smithay::reexports::wayland_server::protocol::wl_seat::WlSeat,
        _serial: smithay::utils::Serial,
    ) {
        if self.pointer_input.freezes_absolute() {
            return;
        }
        log::info!(
            "XDG Move Request received for surface {:?}",
            surface.wl_surface().id()
        );
        let id = surface.wl_surface().id();
        if self.presentation.is_rootless() {
            let _ = self
                .loop_signal
                .send(crate::messages::CompositorMessage::RootlessBeginMove(id));
        } else {
            self.start_drag_request = Some(id);
        }
    }

    fn minimize_request(&mut self, surface: smithay::wayland::shell::xdg::ToplevelSurface) {
        if self.presentation.is_rootless() {
            let _ = self
                .loop_signal
                .send(crate::messages::CompositorMessage::RootlessMinimize(
                    surface.wl_surface().id(),
                ));
        }
    }

    fn title_changed(&mut self, surface: smithay::wayland::shell::xdg::ToplevelSurface) {
        if self.presentation.is_rootless() {
            let _ = self.loop_signal.send(
                crate::messages::CompositorMessage::RootlessToplevelTitleChanged(
                    surface.wl_surface().id(),
                ),
            );
        }
    }

    fn app_id_changed(&mut self, surface: smithay::wayland::shell::xdg::ToplevelSurface) {
        self.title_changed(surface);
    }
}
impl ShmHandler for AppState {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}
impl BufferHandler for AppState {
    fn buffer_destroyed(&mut self, _buffer: &WlBuffer) {}
}
impl SeatHandler for AppState {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;
    fn seat_state(&mut self) -> &mut SeatState<AppState> {
        &mut self.seat_state
    }
    fn cursor_image(&mut self, _seat: &Seat<Self>, image: CursorImageStatus) {
        let cursor_surface = match &image {
            CursorImageStatus::Surface(surface) => Some(surface.clone()),
            _ => None,
        };
        self.native_cursor.set_image(image);
        // Smithay may hold the pointer mutex during this callback. Inspecting
        // current_focus here would deadlock; the loop refreshes locks separately.
        self.native_cursor.refresh();
        if let Some(surface) = cursor_surface {
            // A client can prepare/commit its surface before assigning the
            // cursor role. Those callbacks were queued by the scene path.
            let mut callbacks = Vec::new();
            self.pending_frame_callbacks.retain(|pending| {
                if pending.source_surface == surface {
                    callbacks.push(pending.callback.clone());
                    false
                } else {
                    true
                }
            });
            self.cursor_frame_callbacks.extend(callbacks);
        }
    }
    fn focus_changed(&mut self, seat: &Seat<Self>, focus: Option<&Self::KeyboardFocus>) {
        let client = focus.and_then(Resource::client);
        smithay::wayland::selection::data_device::set_data_device_focus::<Self>(
            &self.display_handle,
            seat,
            client,
        );
    }
}
pub struct ClientState {
    pub compositor_state: CompositorClientState,
}
impl smithay::reexports::wayland_server::backend::ClientData for ClientState {
    fn initialized(&self, _client_id: smithay::reexports::wayland_server::backend::ClientId) {}
    fn disconnected(
        &self,
        _client_id: smithay::reexports::wayland_server::backend::ClientId,
        _reason: smithay::reexports::wayland_server::backend::DisconnectReason,
    ) {
    }
}
use smithay::wayland::selection::data_device::DataDeviceState;
use smithay::wayland::selection::{SelectionSource, SelectionTarget};
impl SelectionHandler for AppState {
    type SelectionUserData = HostImageToken;

    fn new_selection(
        &mut self,
        ty: SelectionTarget,
        source: Option<SelectionSource>,
        _seat: smithay::input::Seat<Self>,
    ) {
        if ty != SelectionTarget::Clipboard {
            return;
        }
        self.guest_clipboard_generation
            .fetch_add(1, Ordering::Relaxed);
        self.host_clipboard_images.replace_pending(None);
        self.pending_host_image_requests.reset(None);
        let source = match source {
            Some(s) => s,
            None => {
                self.pending_guest_clipboard_mime = None;
                return;
            }
        };
        let mime_types = source.mime_types();
        let Some(mime) = preferred_clipboard_mime(&mime_types) else {
            self.pending_guest_clipboard_mime = None;
            return;
        };
        log::info!(
            "Clipboard: Wayland client published {mime}; offered {}",
            mime_types.join(", "),
        );
        crate::diagnostics::record_clipboard_guest_offer(&mime);
        self.pending_guest_clipboard_mime = Some(mime);
    }

    fn send_selection(
        &mut self,
        ty: SelectionTarget,
        mime_type: String,
        fd: std::os::unix::io::OwnedFd,
        _seat: smithay::input::Seat<Self>,
        user_data: &Self::SelectionUserData,
    ) {
        if ty != SelectionTarget::Clipboard {
            return;
        }
        if !user_data.is_current(
            self.guest_clipboard_generation.load(Ordering::Relaxed),
            self.pasteboard_change_count,
            unsafe { objc2_app_kit::NSPasteboard::generalPasteboard().changeCount() },
        ) {
            return;
        }
        if is_clipboard_image_mime(&mime_type)
            && self.host_clipboard.image_png.is_none()
            && self.pending_host_image_requests.token() == Some(*user_data)
        {
            if let Err(error) = self.pending_host_image_requests.retain(*user_data, fd) {
                crate::diagnostics::record_clipboard_host_failure(error.to_string());
            }
            return;
        }
        let payload = if is_clipboard_image_mime(&mime_type) {
            self.host_clipboard
                .image_png
                .clone()
                .map(ClipboardPayload::Image)
        } else if is_clipboard_text_mime(&mime_type) {
            self.host_clipboard.text.clone().map(ClipboardPayload::Text)
        } else {
            return;
        };
        let Some(payload) = payload else {
            return;
        };
        self.send_host_clipboard_payload(fd, payload);
    }
}
impl DataDeviceHandler for AppState {
    fn data_device_state(&mut self) -> &mut DataDeviceState {
        &mut self.data_device_state
    }
}
impl WaylandDndGrabHandler for AppState {}
delegate_data_device!(AppState);
impl smithay::wayland::selection::wlr_data_control::DataControlHandler for AppState {
    fn data_control_state(
        &mut self,
    ) -> &mut smithay::wayland::selection::wlr_data_control::DataControlState {
        &mut self.data_control_state
    }
}
smithay::delegate_data_control!(AppState);
use smithay::delegate_xdg_decoration;
use smithay::wayland::shell::xdg::ToplevelSurface;
impl XdgDecorationHandler for AppState {
    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(DecorationMode::ServerSide);
        });
        toplevel.send_configure();
        log::info!("New decoration requested - using server-side");
    }
    fn request_mode(&mut self, toplevel: ToplevelSurface, mode: DecorationMode) {
        let mode = if self.presentation.is_rootless() {
            DecorationMode::ServerSide
        } else {
            mode
        };
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(mode);
        });
        toplevel.send_configure();
        log::info!("Decoration mode requested: {:?}", mode);
    }
    fn unset_mode(&mut self, toplevel: ToplevelSurface) {
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(DecorationMode::ServerSide);
        });
        toplevel.send_configure();
        log::info!("Decoration mode unset - defaulting to server-side");
    }
}
delegate_xdg_decoration!(AppState);
smithay::delegate_viewporter!(AppState);
impl smithay::wayland::fractional_scale::FractionalScaleHandler for AppState {
    fn new_fractional_scale(
        &mut self,
        surface: smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
    ) {
        smithay::wayland::compositor::with_states(&surface, |states| {
            smithay::wayland::fractional_scale::with_fractional_scale(states, |fs| {
                fs.set_preferred_scale(self.scale_factor);
            });
        });
    }
}
smithay::delegate_fractional_scale!(AppState);
impl smithay::wayland::pointer_constraints::PointerConstraintsHandler for AppState {
    fn new_constraint(
        &mut self,
        surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
        pointer: &smithay::input::pointer::PointerHandle<Self>,
    ) {
        let kind = smithay::wayland::pointer_constraints::with_pointer_constraint::<Self, _, _>(
            surface,
            pointer,
            |constraint| match constraint.as_deref() {
                Some(smithay::wayland::pointer_constraints::PointerConstraint::Locked(_)) => {
                    crate::pointer_input::ConstraintKind::Locked
                }
                Some(smithay::wayland::pointer_constraints::PointerConstraint::Confined(_)) => {
                    crate::pointer_input::ConstraintKind::Confined
                }
                None => crate::pointer_input::ConstraintKind::None,
            },
        );
        self.pointer_input.constraint_created(kind);
        if kind == crate::pointer_input::ConstraintKind::Confined {
            log::warn!(
                "Pointer confinement for {:?} remains inactive: unsupported by the macOS Winit backend",
                surface.id()
            );
        }
    }
    fn cursor_position_hint(
        &mut self,
        _surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
        _pointer: &smithay::input::pointer::PointerHandle<Self>,
        _location: smithay::utils::Point<f64, smithay::utils::Logical>,
    ) {
    }
}
smithay::delegate_pointer_constraints!(AppState);
smithay::delegate_pointer_gestures!(AppState);
smithay::delegate_relative_pointer!(AppState);

smithay::delegate_cursor_shape!(AppState);
impl smithay::wayland::tablet_manager::TabletSeatHandler for AppState {}

fn nix_pipe() -> Option<(std::os::unix::io::OwnedFd, std::os::unix::io::OwnedFd)> {
    use std::os::unix::io::FromRawFd;
    let mut fds = [0i32; 2];
    let ret = unsafe { libc::pipe(fds.as_mut_ptr()) };
    if ret != 0 {
        return None;
    }
    let read = unsafe { std::os::unix::io::OwnedFd::from_raw_fd(fds[0]) };
    let write = unsafe { std::os::unix::io::OwnedFd::from_raw_fd(fds[1]) };
    for fd in fds {
        if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
            return None;
        }
    }
    Some((read, write))
}

fn clipboard_text_mime_types() -> Vec<String> {
    [
        "text/plain;charset=utf-8",
        "text/plain",
        "UTF8_STRING",
        "TEXT",
        "STRING",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

fn read_guest_clipboard(
    fd: std::os::fd::OwnedFd,
    mime: &str,
    generation_counter: &AtomicU64,
    generation: u64,
) -> Result<Option<ClipboardPayload>, String> {
    use std::io::Read;
    use std::os::fd::AsRawFd;
    let mut file = std::fs::File::from(fd);
    let raw = file.as_raw_fd();
    unsafe {
        let flags = libc::fcntl(raw, libc::F_GETFL);
        if flags < 0 || libc::fcntl(raw, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
            return Err("Could not configure clipboard pipe".to_string());
        }
    }
    let limit = if is_clipboard_image_mime(mime) {
        MAX_CLIPBOARD_IMAGE_BYTES
    } else {
        8 * 1024 * 1024
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 16384];
    loop {
        if generation_counter.load(Ordering::Relaxed) != generation {
            return Ok(None);
        }
        if std::time::Instant::now() >= deadline {
            return Err("Wayland clipboard transfer timed out after 10 seconds".to_string());
        }
        match file.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                if bytes.len() + n > limit {
                    return Err(format!(
                        "Wayland clipboard exceeds the {} MiB transfer limit",
                        limit / 1024 / 1024,
                    ));
                }
                bytes.extend_from_slice(&chunk[..n]);
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                let mut poll = libc::pollfd {
                    fd: raw,
                    events: libc::POLLIN,
                    revents: 0,
                };
                if unsafe { libc::poll(&mut poll, 1, 50) } < 0 {
                    let error = std::io::Error::last_os_error();
                    if error.kind() != std::io::ErrorKind::Interrupted {
                        return Err(format!("Clipboard pipe poll failed: {error}"));
                    }
                }
            }
            Err(error) => return Err(format!("Failed to read Wayland clipboard: {error}")),
        }
    }
    if bytes.is_empty() {
        return Ok(None);
    }
    if is_clipboard_image_mime(mime) {
        let image = shared_png_bytes(bytes)?.ok_or("Empty Wayland PNG")?;
        // Validate the complete image on the worker, not the AppKit event loop.
        objc2::rc::autoreleasepool(|_| unsafe {
            let data = objc2_foundation::NSData::with_bytes(&image);
            let decoded = objc2_app_kit::NSBitmapImageRep::imageRepWithData(&data)
                .ok_or("Wayland clipboard PNG could not be decoded")?;
            validate_clipboard_image_dimensions(decoded.pixelsWide(), decoded.pixelsHigh())
        })?;
        Ok(Some(ClipboardPayload::Image(image)))
    } else {
        let text = String::from_utf8(bytes).map_err(|_| "Wayland clipboard text is not UTF-8")?;
        Ok(Some(ClipboardPayload::Text(Arc::from(text))))
    }
}

fn write_clipboard_payload(fd: std::os::fd::OwnedFd, mut bytes: &[u8]) -> std::io::Result<()> {
    use std::io::{Error, ErrorKind, Write};
    use std::os::fd::AsRawFd;
    let mut file = std::fs::File::from(fd);
    let raw = file.as_raw_fd();
    unsafe {
        let flags = libc::fcntl(raw, libc::F_GETFL);
        if flags < 0 || libc::fcntl(raw, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
            return Err(Error::last_os_error());
        }
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !bytes.is_empty() {
        if std::time::Instant::now() >= deadline {
            return Err(Error::new(
                ErrorKind::TimedOut,
                "clipboard transfer exceeded 10 seconds",
            ));
        }
        match file.write(bytes) {
            Ok(0) => {
                return Err(Error::new(
                    ErrorKind::WriteZero,
                    "clipboard receiver stopped reading",
                ));
            }
            Ok(n) => bytes = &bytes[n..],
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                let mut poll = libc::pollfd {
                    fd: raw,
                    events: libc::POLLOUT,
                    revents: 0,
                };
                if unsafe { libc::poll(&mut poll, 1, 50) } < 0 {
                    let error = Error::last_os_error();
                    if error.kind() != ErrorKind::Interrupted {
                        return Err(error);
                    }
                }
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn preferred_clipboard_mime(mime_types: &[String]) -> Option<String> {
    mime_types
        .iter()
        .find(|mime| is_clipboard_image_mime(mime))
        .cloned()
        .or_else(|| preferred_clipboard_text_mime(mime_types))
}

fn clipboard_mime_types(has_text: bool, has_image_png: bool) -> Vec<String> {
    let mut mime_types = Vec::new();
    if has_image_png {
        mime_types.push(CLIPBOARD_IMAGE_MIME.to_owned());
    }
    if has_text {
        mime_types.extend(clipboard_text_mime_types());
    }
    mime_types
}

fn is_clipboard_image_mime(mime: &str) -> bool {
    mime.trim().eq_ignore_ascii_case(CLIPBOARD_IMAGE_MIME)
}

fn is_clipboard_text_mime(mime: &str) -> bool {
    let normalized = mime.trim().to_ascii_lowercase();
    normalized == "utf8_string"
        || normalized == "text"
        || normalized == "string"
        || normalized == "text/plain"
        || normalized.starts_with("text/plain;")
}

fn preferred_clipboard_text_mime(mime_types: &[String]) -> Option<String> {
    const PRIORITIES: [&str; 5] = [
        "text/plain;charset=utf-8",
        "text/plain",
        "utf8_string",
        "text",
        "string",
    ];
    PRIORITIES
        .iter()
        .find_map(|preferred| {
            mime_types
                .iter()
                .find(|mime| mime.trim().eq_ignore_ascii_case(preferred))
                .cloned()
        })
        .or_else(|| {
            mime_types
                .iter()
                .find(|mime| is_clipboard_text_mime(mime))
                .cloned()
        })
}

#[cfg(test)]
mod surface_ancestry_tests {
    use super::{MAX_SURFACE_ANCESTORS, bounded_parent_chain};

    #[test]
    fn accepts_a_root_and_mixed_popup_subsurface_ancestry() {
        assert_eq!(bounded_parent_chain(0, |id| *id, |_| None), Some(vec![0]));
        assert_eq!(
            bounded_parent_chain(
                0,
                |id| *id,
                |id| match id {
                    0 => Some(1), // subsurface -> popup
                    1 => Some(2), // popup -> subsurface
                    2 => Some(3), // subsurface -> toplevel
                    _ => None,
                }
            ),
            Some(vec![0, 1, 2, 3])
        );
    }

    #[test]
    fn rejects_self_and_two_node_cycles_before_revisiting_a_parent() {
        let mut calls = 0;
        assert!(
            bounded_parent_chain(
                0,
                |id| *id,
                |_| {
                    calls += 1;
                    Some(0)
                }
            )
            .is_none()
        );
        assert_eq!(calls, 1);
        calls = 0;
        assert!(
            bounded_parent_chain(
                0,
                |id| *id,
                |id| {
                    calls += 1;
                    Some(1 - id)
                }
            )
            .is_none()
        );
        assert_eq!(calls, 2);
    }

    #[test]
    fn accepts_the_depth_boundary_and_bounds_acyclic_walks() {
        let chain = bounded_parent_chain(
            0,
            |id| *id,
            |id| (*id + 1 < MAX_SURFACE_ANCESTORS).then_some(id + 1),
        )
        .unwrap();
        assert_eq!(chain.len(), MAX_SURFACE_ANCESTORS);
        let mut calls = 0;
        assert!(
            bounded_parent_chain(
                0,
                |id| *id,
                |id| {
                    calls += 1;
                    Some(id + 1)
                }
            )
            .is_none()
        );
        assert_eq!(calls, MAX_SURFACE_ANCESTORS);
        // A rejected client's graph does not poison the next independent walk.
        assert_eq!(bounded_parent_chain(5, |id| *id, |_| None), Some(vec![5]));
    }
}

#[cfg(test)]
mod role_lifecycle_tests {
    use super::*;
    use crate::messages::CompositorMessage;
    use crate::presentation::PresentationMode;
    use smithay::reexports::wayland_server::Display;
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    use std::sync::mpsc;
    use std::time::Duration;

    // An in-process protocol peer exercises the real destruction callbacks without
    // opening native windows or requiring client-side XDG protocol features.
    struct Scene {
        display: Display<AppState>,
        state: AppState,
        peer: UnixStream,
        messages: mpsc::Receiver<CompositorMessage>,
        next_id: u32,
        compositor: u32,
        shell: u32,
    }

    fn word(bytes: &[u8]) -> u32 {
        u32::from_ne_bytes(bytes[..4].try_into().unwrap())
    }

    impl Scene {
        fn new(mode: PresentationMode) -> Self {
            let display = Display::new().unwrap();
            let (sender, messages) = mpsc::channel();
            let state = AppState::new(
                &display.handle(),
                1.0,
                crate::event_loop_signal::Sender::with_waker(sender, || {}),
                mode,
                800,
                600,
            )
            .unwrap();
            let (peer, server) = UnixStream::pair().unwrap();
            peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            peer.set_write_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            display
                .handle()
                .insert_client(
                    server,
                    Arc::new(ClientState {
                        compositor_state: CompositorClientState::default(),
                    }),
                )
                .unwrap();
            let mut scene = Self {
                display,
                state,
                peer,
                messages,
                next_id: 3,
                compositor: 0,
                shell: 0,
            };
            scene.request(1, 1, &[2]); // wl_display.get_registry
            let globals = scene.roundtrip();
            for (object, opcode, body) in globals {
                if object == 2 && opcode == 0 {
                    let length = word(&body[4..]) as usize;
                    let interface = std::str::from_utf8(&body[8..8 + length - 1]).unwrap();
                    if interface == "wl_compositor" {
                        scene.compositor = scene.bind(word(&body), interface);
                    } else if interface == "xdg_wm_base" {
                        scene.shell = scene.bind(word(&body), interface);
                    }
                }
            }
            assert_ne!(scene.compositor, 0);
            assert_ne!(scene.shell, 0);
            scene.roundtrip();
            scene
        }

        fn id(&mut self) -> u32 {
            let id = self.next_id;
            self.next_id += 1;
            id
        }

        fn send(&mut self, object: u32, opcode: u16, body: &[u8]) {
            let mut bytes = object.to_ne_bytes().to_vec();
            bytes.extend((((body.len() as u32 + 8) << 16) | u32::from(opcode)).to_ne_bytes());
            bytes.extend(body);
            self.peer.write_all(&bytes).unwrap();
        }

        fn request(&mut self, object: u32, opcode: u16, words: &[u32]) {
            let body: Vec<_> = words.iter().flat_map(|word| word.to_ne_bytes()).collect();
            self.send(object, opcode, &body);
        }

        fn bind(&mut self, global: u32, interface: &str) -> u32 {
            let id = self.id();
            let mut body = global.to_ne_bytes().to_vec();
            body.extend(((interface.len() + 1) as u32).to_ne_bytes());
            body.extend(interface.as_bytes());
            body.push(0);
            body.resize(body.len().next_multiple_of(4), 0);
            body.extend(1u32.to_ne_bytes());
            body.extend(id.to_ne_bytes());
            self.send(2, 0, &body);
            id
        }

        fn roundtrip(&mut self) -> Vec<(u32, u16, Vec<u8>)> {
            let callback = self.id();
            self.request(1, 0, &[callback]);
            self.display.dispatch_clients(&mut self.state).unwrap();
            self.display.flush_clients().unwrap();
            let mut events = Vec::new();
            loop {
                let mut header = [0; 8];
                self.peer.read_exact(&mut header).unwrap();
                let object = word(&header);
                let opcode = word(&header[4..]) as u16;
                let size = (word(&header[4..]) >> 16) as usize;
                assert!(size >= 8);
                let mut body = vec![0; size - 8];
                self.peer.read_exact(&mut body).unwrap();
                assert!(!(object == 1 && opcode == 0), "wl_display.error: {body:?}");
                if object == callback && opcode == 0 {
                    return events;
                }
                events.push((object, opcode, body));
            }
        }

        fn xdg_surface(&mut self) -> (u32, u32) {
            let wl = self.id();
            self.request(self.compositor, 0, &[wl]);
            let xdg = self.id();
            self.request(self.shell, 2, &[xdg, wl]);
            (wl, xdg)
        }

        fn toplevel(&mut self) -> (u32, u32, u32) {
            let (wl, xdg) = self.xdg_surface();
            let role = self.id();
            self.request(xdg, 1, &[role]);
            self.roundtrip();
            (wl, xdg, role)
        }

        fn popup(&mut self, xdg: u32, parent: u32, positioner: u32) -> u32 {
            let role = self.id();
            self.request(xdg, 2, &[role, parent, positioner]);
            self.roundtrip();
            role
        }

        fn ack_configure(&mut self, xdg: u32, events: &[(u32, u16, Vec<u8>)]) {
            let (_, _, body) = events
                .iter()
                .rev()
                .find(|(object, opcode, _)| *object == xdg && *opcode == 0)
                .expect("xdg_surface.configure must precede the first commit");
            self.request(xdg, 4, &[word(body)]); // xdg_surface.ack_configure
            self.roundtrip();
        }

        fn configured_toplevel(&mut self) -> (WlSurface, u32) {
            let (wl, xdg) = self.xdg_surface();
            let role = self.id();
            self.request(xdg, 1, &[role]);
            let events = self.roundtrip();
            self.ack_configure(xdg, &events);
            let surface = self
                .state
                .toplevels
                .iter()
                .find(|toplevel| toplevel.wl_surface().id().protocol_id() == wl)
                .unwrap()
                .wl_surface()
                .clone();
            (surface, xdg)
        }

        fn queue_frame_callbacks(&mut self, surface: &WlSurface, count: usize) -> Vec<u32> {
            let callbacks = (0..count)
                .map(|_| {
                    let callback = self.id();
                    self.request(surface.id().protocol_id(), 3, &[callback]); // wl_surface.frame
                    callback
                })
                .collect();
            self.request(surface.id().protocol_id(), 6, &[]); // wl_surface.commit
            callbacks
        }

        fn pending_frame_ids(&self) -> Vec<u32> {
            self.state
                .pending_frame_callbacks
                .iter()
                .map(|pending| pending.callback.id().protocol_id())
                .collect()
        }

        fn seed_scene_state(&mut self, surface: &WlSurface, root: &WlSurface) {
            let callback = self.id();
            self.request(surface.id().protocol_id(), 3, &[callback]); // wl_surface.frame
            self.roundtrip();
            let callback = smithay::wayland::compositor::with_states(surface, |states| {
                states
                    .cached_state
                    .get::<smithay::wayland::compositor::SurfaceAttributes>()
                    .pending()
                    .frame_callbacks
                    .pop()
                    .unwrap()
            });
            self.state
                .pending_frame_callbacks
                .push(PendingFrameCallback {
                    root_surface: Some(root.id()),
                    source_surface: surface.clone(),
                    callback,
                    trace_commit: None,
                });
            self.state.surface_positions.insert(surface.id(), (1, 2));
            self.state
                .pointer_surface_buffers
                .insert(surface.id(), (10, 10));
            self.state.drag_state = Some((surface.id(), (1.0, 2.0)));
            self.state.start_drag_request = Some(surface.id());
            let keyboard = self.state.seat.get_keyboard().unwrap();
            keyboard.set_focus(
                &mut self.state,
                Some(surface.clone()),
                smithay::utils::SERIAL_COUNTER.next_serial(),
            );
        }

        fn assert_retired(&self, surface: &WlSurface) {
            assert!(!self.state.surface_positions.contains_key(&surface.id()));
            assert!(
                !self
                    .state
                    .pointer_surface_buffers
                    .contains_key(&surface.id())
            );
            assert!(self.state.pending_frame_callbacks.is_empty());
            assert!(self.state.drag_state.is_none());
            assert!(self.state.start_drag_request.is_none());
            assert!(
                self.state
                    .seat
                    .get_keyboard()
                    .unwrap()
                    .current_focus()
                    .is_none()
            );
        }
    }

    fn frame_done_events(events: &[(u32, u16, Vec<u8>)], callbacks: &[u32]) -> Vec<(u32, u32)> {
        events
            .iter()
            .filter_map(|(object, opcode, body)| {
                if *opcode != 0 || !callbacks.contains(object) {
                    return None;
                }
                assert_eq!(body.len(), 4, "wl_callback.done has exactly one timestamp");
                Some((*object, word(body)))
            })
            .collect()
    }

    #[test]
    fn desktop_ready_root_and_popup_ack_without_retiring_blocked_root() {
        let mut scene = Scene::new(PresentationMode::Desktop);
        let (blocked, _) = scene.configured_toplevel();
        let (ready, ready_xdg) = scene.configured_toplevel();
        let blocked_root = blocked.id();
        let ready_root = ready.id();

        let (_, popup_xdg) = scene.xdg_surface();
        let positioner = scene.id();
        scene.request(scene.shell, 1, &[positioner]);
        scene.request(positioner, 1, &[4, 4]);
        scene.request(positioner, 2, &[0, 0, 4, 4]);
        let popup_role = scene.id();
        scene.request(popup_xdg, 2, &[popup_role, ready_xdg, positioner]);
        let events = scene.roundtrip();
        scene.ack_configure(popup_xdg, &events);
        let popup = scene.state.popups[0].wl_surface().clone();
        assert_eq!(
            scene.state.root_toplevel_id_for_surface(&popup),
            Some(ready_root.clone())
        );

        // Interleave real frame requests and commits; do not seed the callback queue.
        let a_first = scene.queue_frame_callbacks(&blocked, 1)[0];
        let b = scene.queue_frame_callbacks(&ready, 2);
        let popup_first = scene.queue_frame_callbacks(&popup, 1)[0];
        let a_second = scene.queue_frame_callbacks(&blocked, 1)[0];
        let popup_second = scene.queue_frame_callbacks(&popup, 1)[0];
        let mut callbacks = vec![a_first, b[0], b[1], popup_first, a_second, popup_second];
        let events = scene.roundtrip();
        assert!(frame_done_events(&events, &callbacks).is_empty());
        assert_eq!(scene.pending_frame_ids(), callbacks);
        for pending in &scene.state.pending_frame_callbacks {
            let root = if pending.source_surface == blocked {
                &blocked_root
            } else {
                &ready_root
            };
            assert_eq!(pending.root_surface.as_ref(), Some(root));
        }

        // Tracing is disabled in this headless harness; seed only the owner-keyed
        // trace bookkeeping, while callbacks and their ordering come from Wayland.
        scene.state.trace_frames.insert(blocked_root.clone(), 11);
        scene.state.trace_frames.insert(ready_root.clone(), 21);
        let ready_traces = scene.state.trace_frames_for(Some(&ready_root));
        assert_eq!(ready_traces, [21]);
        assert_eq!(scene.state.trace_frames_for(Some(&ready_root)), [21]);
        scene.state.retire_trace_frames(&ready_traces);
        scene.state.send_frame_callbacks_for(Some(&ready_root), 100);
        let events = scene.roundtrip();
        assert_eq!(
            frame_done_events(&events, &callbacks),
            [
                (b[0], 100),
                (b[1], 100),
                (popup_first, 100),
                (popup_second, 100)
            ]
        );
        assert_eq!(scene.pending_frame_ids(), [a_first, a_second]);
        assert!(
            scene.state.pending_frame_callbacks.iter().all(|pending| {
                pending.source_surface == blocked && pending.callback.is_alive()
            })
        );
        assert!(scene.state.trace_frames_for(Some(&ready_root)).is_empty());
        assert_eq!(scene.state.trace_frames_for(Some(&blocked_root)), [11]);
        assert_eq!(scene.state.trace_frames.len(), 1);

        // Retrying the ready root cannot re-ack B or drain the blocked owner's queue.
        scene.state.send_frame_callbacks_for(Some(&ready_root), 101);
        let events = scene.roundtrip();
        assert!(frame_done_events(&events, &callbacks).is_empty());
        assert_eq!(scene.pending_frame_ids(), [a_first, a_second]);
        assert_eq!(scene.state.trace_frames_for(Some(&blocked_root)), [11]);

        // A newer blocked commit must append, not replace, its older callbacks.
        let a_latest = scene.queue_frame_callbacks(&blocked, 1)[0];
        callbacks.push(a_latest);
        let events = scene.roundtrip();
        assert!(frame_done_events(&events, &callbacks).is_empty());
        assert_eq!(scene.pending_frame_ids(), [a_first, a_second, a_latest]);
        scene.state.trace_frames.insert(blocked_root.clone(), 12);
        scene.state.retire_trace_frames(&ready_traces); // stale B retirement is harmless to A
        assert_eq!(scene.state.trace_frames_for(Some(&blocked_root)), [12]);

        let blocked_traces = scene.state.trace_frames_for(Some(&blocked_root));
        assert_eq!(blocked_traces, [12]);
        scene.state.retire_trace_frames(&blocked_traces);
        scene
            .state
            .send_frame_callbacks_for(Some(&blocked_root), 200);
        let events = scene.roundtrip();
        assert_eq!(
            frame_done_events(&events, &callbacks),
            [(a_first, 200), (a_second, 200), (a_latest, 200)]
        );
        assert!(scene.state.pending_frame_callbacks.is_empty());
        assert!(scene.state.trace_frames_for(None).is_empty());

        scene
            .state
            .send_frame_callbacks_for(Some(&blocked_root), 201);
        scene.state.send_frame_callbacks_for(Some(&ready_root), 201);
        scene.state.send_frame_callbacks_for(None, 201);
        let events = scene.roundtrip();
        assert!(frame_done_events(&events, &callbacks).is_empty());
    }

    #[test]
    fn deferred_trace_ids_survive_retries_and_newer_commits() {
        let mut scene = Scene::new(PresentationMode::Desktop);
        scene.toplevel();
        let root = scene.state.toplevels[0].wl_surface().id();
        scene.state.trace_frames.insert(root.clone(), 11);
        assert_eq!(scene.state.trace_frames_for(Some(&root)), [11]);
        assert_eq!(scene.state.trace_frames_for(Some(&root)), [11]);
        scene.state.trace_frames.insert(root.clone(), 12);
        scene.state.retire_trace_frames(&[11]);
        assert_eq!(scene.state.trace_frames_for(Some(&root)), [12]);
        scene.state.retire_trace_frames(&[12]);
        assert!(scene.state.trace_frames_for(None).is_empty());
    }

    #[test]
    fn toplevel_role_destruction_and_recreation_are_independent_of_wl_surface() {
        for mode in [PresentationMode::Desktop, PresentationMode::Rootless] {
            let mut scene = Scene::new(mode);
            let (wl, xdg, role) = scene.toplevel();
            let old = scene.state.toplevels[0].clone();
            let surface = old.wl_surface().clone();
            scene.seed_scene_state(&surface, &surface);
            scene.state.trace_frames.insert(surface.id(), 1);
            let _ = scene.messages.try_iter().collect::<Vec<_>>();
            scene.request(role, 0, &[]);
            scene.roundtrip();
            assert!(surface.is_alive());
            assert!(scene.state.toplevels.is_empty());
            assert!(scene.state.layout.tiles.is_empty());
            assert!(!scene.state.rootless_dirty_surfaces.contains(&surface.id()));
            assert!(!scene.state.trace_frames.contains_key(&surface.id()));
            scene.assert_retired(&surface);
            let messages: Vec<_> = scene.messages.try_iter().collect();
            assert_eq!(messages.iter().filter(|message| matches!(
                message, CompositorMessage::RootlessToplevelDestroyed(id) if id == &surface.id()
            )).count(), usize::from(mode.is_rootless()));
            assert!(
                !messages
                    .iter()
                    .any(|message| matches!(message, CompositorMessage::SurfaceDestroyed(_)))
            );

            let replacement = scene.id();
            scene.request(xdg, 1, &[replacement]);
            scene.roundtrip();
            assert_eq!(scene.state.toplevels.len(), 1);
            assert_ne!(scene.state.toplevels[0].xdg_toplevel(), old.xdg_toplevel());
            assert_eq!(scene.state.toplevels[0].wl_surface(), &surface);
            if mode == PresentationMode::Desktop {
                assert_eq!(
                    scene.state.layout.tiles[0]
                        .toplevel
                        .xdg_toplevel()
                        .id()
                        .protocol_id(),
                    replacement
                );
            }
            scene.state.toplevel_destroyed(old); // stale role identity must be harmless
            assert_eq!(scene.state.toplevels.len(), 1);
            scene.request(replacement, 0, &[]);
            scene.request(xdg, 0, &[]);
            scene.request(wl, 0, &[]);
            scene.roundtrip();
            assert_eq!(
                scene
                    .messages
                    .try_iter()
                    .filter(|message| matches!(
                        message, CompositorMessage::SurfaceDestroyed(id) if id == &surface.id()
                    ))
                    .count(),
                1
            );
        }
    }

    #[test]
    fn popup_role_destruction_repaints_its_owner_and_allows_recreation() {
        let mut scene = Scene::new(PresentationMode::Rootless);
        let (_, parent, _) = scene.toplevel();
        let root = scene.state.toplevels[0].wl_surface().clone();
        let (_, xdg) = scene.xdg_surface();
        let positioner = scene.id();
        scene.request(scene.shell, 1, &[positioner]);
        scene.request(positioner, 1, &[4, 4]);
        scene.request(positioner, 2, &[0, 0, 4, 4]);
        let role = scene.popup(xdg, parent, positioner);
        let old = scene.state.popups[0].clone();
        let surface = old.wl_surface().clone();
        scene.seed_scene_state(&surface, &root);
        scene.state.rootless_dirty_surfaces.clear();
        let _ = scene.messages.try_iter().collect::<Vec<_>>();
        scene.request(role, 0, &[]);
        scene.roundtrip();
        assert!(surface.is_alive());
        assert!(scene.state.popups.is_empty());
        assert_eq!(scene.state.toplevels.len(), 1);
        assert!(scene.state.rootless_dirty_surfaces.contains(&root.id()));
        assert!(scene.state.needs_redraw);
        scene.assert_retired(&surface);
        assert!(scene.messages.try_iter().next().is_none());
        scene.popup(xdg, parent, positioner);
        assert_eq!(scene.state.popups.len(), 1);
        assert_ne!(scene.state.popups[0].xdg_popup(), old.xdg_popup());
        scene.state.popup_destroyed(old);
        assert_eq!(scene.state.popups.len(), 1);
    }

    #[test]
    fn every_non_cursor_wl_surface_destruction_notifies_both_presentations() {
        for mode in [PresentationMode::Desktop, PresentationMode::Rootless] {
            let mut scene = Scene::new(mode);
            let wl = scene.id();
            scene.request(scene.compositor, 0, &[wl]);
            scene.request(wl, 0, &[]);
            scene.roundtrip();
            assert_eq!(
                scene
                    .messages
                    .try_iter()
                    .filter(|message| matches!(
                        message, CompositorMessage::SurfaceDestroyed(id) if id.protocol_id() == wl
                    ))
                    .count(),
                1
            );
        }
    }
}

#[cfg(test)]
mod pointer_axis_tests {
    use super::PointerAxisTracker;
    use smithay::backend::input::AxisSource;
    use winit::{
        dpi::PhysicalPosition,
        event::{MouseScrollDelta, TouchPhase},
    };

    #[test]
    fn preserves_both_axes_for_trackpad_scrolls() {
        let mut tracker = PointerAxisTracker::default();
        let frame = tracker
            .frame(
                2.0,
                MouseScrollDelta::PixelDelta(PhysicalPosition::new(12.0, -8.0)),
                TouchPhase::Started,
                10,
            )
            .expect("non-zero trackpad movement should produce a frame");

        assert_eq!(frame.source, Some(AxisSource::Finger));
        assert_eq!(frame.axis, (-1.5, 1.0));
        assert_eq!(frame.stop, (false, false));
        assert_eq!(frame.v120, None);
    }

    #[test]
    fn trackpad_scrolls_use_pixel_pan_instead_of_trackpoint_lines() {
        let mut tracker = PointerAxisTracker::default();
        for phase in [TouchPhase::Started, TouchPhase::Moved, TouchPhase::Moved] {
            let frame = tracker
                .frame(
                    2.0,
                    MouseScrollDelta::PixelDelta(PhysicalPosition::new(0.0, 4.0)),
                    phase,
                    10,
                )
                .expect("non-zero trackpad movement should produce a frame");
            assert_eq!(frame.source, Some(AxisSource::Finger));
            // GTK divides axis units by 10, then Firefox expands them by 40.
            assert_eq!(-frame.axis.1 / 10.0 * 40.0, 2.0);
        }

        let end = tracker.frame(
            2.0,
            MouseScrollDelta::PixelDelta(PhysicalPosition::new(0.0, 0.0)),
            TouchPhase::Ended,
            11,
        );
        assert_eq!(end.unwrap().stop, (false, true));
    }

    #[test]
    fn zero_delta_end_stops_every_active_trackpad_axis_once() {
        let mut tracker = PointerAxisTracker::default();
        tracker.frame(
            1.0,
            MouseScrollDelta::PixelDelta(PhysicalPosition::new(3.0, 5.0)),
            TouchPhase::Started,
            10,
        );
        let end = tracker.frame(
            1.0,
            MouseScrollDelta::PixelDelta(PhysicalPosition::new(0.0, 0.0)),
            TouchPhase::Ended,
            11,
        );
        let end = end.expect("finger release must stop both active axes");
        assert_eq!(end.axis, (0.0, 0.0));
        assert_eq!(end.stop, (true, true));
        assert!(
            tracker
                .frame(
                    1.0,
                    MouseScrollDelta::PixelDelta(PhysicalPosition::new(0.0, 0.0)),
                    TouchPhase::Ended,
                    12,
                )
                .is_none()
        );
    }

    #[test]
    fn cancelling_a_gesture_stops_its_active_axis() {
        let mut tracker = PointerAxisTracker::default();
        tracker.frame(
            1.0,
            MouseScrollDelta::PixelDelta(PhysicalPosition::new(3.0, 0.0)),
            TouchPhase::Started,
            10,
        );
        let cancelled = tracker
            .frame(
                1.0,
                MouseScrollDelta::PixelDelta(PhysicalPosition::new(0.0, 0.0)),
                TouchPhase::Cancelled,
                11,
            )
            .expect("cancellation must stop the active axis");
        assert_eq!(cancelled.stop, (true, false));
    }

    #[test]
    fn trackpad_distance_is_independent_of_hidpi_scale() {
        for scale in [1.0, 1.5, 2.0, 3.0] {
            let mut tracker = PointerAxisTracker::default();
            let frame = tracker
                .frame(
                    scale,
                    MouseScrollDelta::PixelDelta(PhysicalPosition::new(8.0 * scale, -40.0 * scale)),
                    TouchPhase::Moved,
                    10,
                )
                .unwrap();
            assert_eq!(frame.axis, (-2.0, 10.0));
            assert_eq!(frame.v120, None);
        }
    }

    #[test]
    fn small_trackpad_deltas_are_not_rounded_to_wheel_ticks() {
        let mut tracker = PointerAxisTracker::default();
        let frame = tracker
            .frame(
                2.0,
                MouseScrollDelta::PixelDelta(PhysicalPosition::new(0.125, -0.25)),
                TouchPhase::Moved,
                10,
            )
            .unwrap();
        assert_eq!(frame.axis, (-0.015625, 0.03125));
        assert_eq!(frame.v120, None);
    }

    #[test]
    fn mouse_wheel_keeps_v120_and_does_not_emit_finger_stops() {
        let mut tracker = PointerAxisTracker::default();
        let frame = tracker
            .frame(
                1.0,
                MouseScrollDelta::LineDelta(1.0, -2.0),
                TouchPhase::Moved,
                10,
            )
            .expect("wheel movement should produce a frame");

        assert_eq!(frame.source, Some(AxisSource::Wheel));
        assert_eq!(frame.axis, (-10.0, 20.0));
        assert_eq!(frame.v120, Some((-120, 240)));
        assert_eq!(frame.stop, (false, false));
    }
}

#[cfg(test)]
mod clipboard_tests {
    use super::{
        CLIPBOARD_IMAGE_MIME, ClipboardPayload, MAX_CLIPBOARD_IMAGE_BYTES, clipboard_mime_types,
        is_clipboard_image_mime, is_clipboard_text_mime, png_dimensions, preferred_clipboard_mime,
        preferred_clipboard_text_mime, read_guest_clipboard, shared_png_bytes,
    };

    fn sample_png(width: u32, height: u32) -> Vec<u8> {
        let mut png = vec![0; 24];
        png[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
        png[12..16].copy_from_slice(b"IHDR");
        png[16..20].copy_from_slice(&width.to_be_bytes());
        png[20..24].copy_from_slice(&height.to_be_bytes());
        png
    }

    #[test]
    fn advertises_png_before_text_when_both_are_available() {
        let offered = clipboard_mime_types(true, true);
        assert_eq!(
            offered.first().map(String::as_str),
            Some(CLIPBOARD_IMAGE_MIME)
        );
        assert!(offered.iter().any(|mime| mime == "text/plain"));
    }

    #[test]
    fn clears_all_mime_types_when_the_host_clipboard_is_empty() {
        assert!(clipboard_mime_types(false, false).is_empty());
    }

    #[test]
    fn accepts_png_without_treating_it_as_text() {
        assert!(is_clipboard_image_mime("IMAGE/PNG"));
        assert!(!is_clipboard_text_mime(CLIPBOARD_IMAGE_MIME));
    }

    #[test]
    fn validates_screenshot_dimensions_and_shares_the_encoded_bytes() {
        let png = sample_png(6016, 3384);
        assert_eq!(png_dimensions(&png), Some((6016, 3384)));
        let shared = shared_png_bytes(png.clone()).unwrap().unwrap();
        assert_eq!(&*shared, png.as_slice());
    }

    #[test]
    fn rejects_oversized_png_clipboards() {
        let mut png = sample_png(1, 1);
        png.resize(MAX_CLIPBOARD_IMAGE_BYTES + 1, 0);
        assert!(shared_png_bytes(png).unwrap_err().contains("64 MiB"));
    }

    #[test]
    fn image_bounds_reject_zero_overflow_and_decompression_bombs() {
        use super::{
            MAX_CLIPBOARD_IMAGE_PIXELS, MAX_CLIPBOARD_IMAGE_SOURCE_BYTES,
            validate_clipboard_image_dimensions, validate_clipboard_source_size,
        };
        assert!(validate_clipboard_source_size(MAX_CLIPBOARD_IMAGE_SOURCE_BYTES).is_ok());
        assert!(validate_clipboard_source_size(MAX_CLIPBOARD_IMAGE_SOURCE_BYTES + 1).is_err());
        assert!(validate_clipboard_source_size(usize::MAX).is_err());
        assert!(
            validate_clipboard_image_dimensions(MAX_CLIPBOARD_IMAGE_PIXELS as isize, 1).is_ok()
        );
        for (width, height) in [(0, 1), (1, 0), (-1, 1), (isize::MAX, 3), (65536, 65536)] {
            assert!(validate_clipboard_image_dimensions(width, height).is_err());
        }
        assert!(shared_png_bytes(sample_png(0, 1)).is_err());
        assert!(shared_png_bytes(sample_png(65536, 65536)).is_err());
    }

    fn sample_tiff(width: u32, height: u32, little: bool) -> Vec<u8> {
        let u16_bytes = |value: u16| {
            if little {
                value.to_le_bytes()
            } else {
                value.to_be_bytes()
            }
        };
        let u32_bytes = |value: u32| {
            if little {
                value.to_le_bytes()
            } else {
                value.to_be_bytes()
            }
        };
        let mut bytes = if little {
            b"II\x2a\0".to_vec()
        } else {
            b"MM\0\x2a".to_vec()
        };
        bytes.extend_from_slice(&u32_bytes(8));
        bytes.extend_from_slice(&u16_bytes(4));
        for (tag, kind, value) in [(256, 4, width), (257, 4, height), (258, 3, 8), (277, 3, 4)] {
            bytes.extend_from_slice(&u16_bytes(tag));
            bytes.extend_from_slice(&u16_bytes(kind));
            bytes.extend_from_slice(&u32_bytes(1));
            if kind == 3 {
                bytes.extend_from_slice(&u16_bytes(value as u16));
                bytes.extend_from_slice(&[0; 2]);
            } else {
                bytes.extend_from_slice(&u32_bytes(value));
            }
        }
        bytes.extend_from_slice(&u32_bytes(0));
        bytes
    }

    #[test]
    fn tiff_preflight_bounds_both_byte_orders_before_decoding() {
        for little in [true, false] {
            assert!(super::validate_clipboard_tiff(&sample_tiff(6016, 3384, little)).is_ok());
            assert!(super::validate_clipboard_tiff(&sample_tiff(8192, 4096, little)).is_ok());
            assert!(super::validate_clipboard_tiff(&sample_tiff(8193, 4096, little)).is_err());
            assert!(super::validate_clipboard_tiff(&sample_tiff(0, 1, little)).is_err());
        }
    }

    #[test]
    fn tiff_preflight_rejects_malformed_offsets_frames_and_channel_depths() {
        let original = sample_tiff(1, 1, true);
        for end in 0..original.len() {
            assert!(super::validate_clipboard_tiff(&original[..end]).is_err());
        }
        let mut malformed = original.clone();
        malformed[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(super::validate_clipboard_tiff(&malformed).is_err());
        let mut multiframe = original.clone();
        multiframe[58..62].copy_from_slice(&8u32.to_le_bytes());
        assert!(
            super::validate_clipboard_tiff(&multiframe)
                .unwrap_err()
                .contains("multi-image")
        );
        let mut deep = original.clone();
        deep[42..44].copy_from_slice(&32u16.to_le_bytes());
        assert!(
            super::validate_clipboard_tiff(&deep)
                .unwrap_err()
                .contains("16-bit")
        );
        let mut channels = original;
        channels[54..56].copy_from_slice(&5u16.to_le_bytes());
        assert!(
            super::validate_clipboard_tiff(&channels)
                .unwrap_err()
                .contains("4-channel")
        );
        assert!(super::validate_clipboard_tiff(b"II\x2b\0\x08\0\0\0").is_err());
    }

    #[test]
    fn tiff_preflight_checks_external_channel_metadata_without_decoding() {
        let mut tiff = sample_tiff(6016, 3384, true);
        tiff[38..42].copy_from_slice(&4u32.to_le_bytes());
        tiff[42..46].copy_from_slice(&62u32.to_le_bytes());
        for _ in 0..4 {
            tiff.extend_from_slice(&8u16.to_le_bytes());
        }
        assert!(super::validate_clipboard_tiff(&tiff).is_ok());
        assert!(super::validate_clipboard_tiff(&tiff[..tiff.len() - 1]).is_err());
        tiff[42..46].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(super::validate_clipboard_tiff(&tiff).is_err());
    }

    #[test]
    fn appkit_tiff_round_trip_runs_off_thread_without_accessing_the_pasteboard() {
        use objc2_app_kit::{NSBitmapImageRep, NSTIFFFileType};
        use objc2_foundation::{NSData, NSDictionary};

        let tiff = objc2::rc::autoreleasepool(|_| unsafe {
            let png = NSData::with_bytes(include_bytes!("../assets/icon.png"));
            let image = NSBitmapImageRep::imageRepWithData(&png).unwrap();
            let tiff = image
                .representationUsingType_properties(NSTIFFFileType, &NSDictionary::new())
                .unwrap();
            tiff.bytes().to_vec().into_boxed_slice()
        });
        let png = std::thread::spawn(move || super::convert_host_clipboard_tiff(tiff))
            .join()
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(super::shared_png_bytes(png.to_vec()).unwrap().is_some());
    }

    #[test]
    fn chooses_the_exact_mime_offered_by_the_client() {
        let offered = vec![
            "image/png".to_string(),
            "text/plain;charset=UTF-8".to_string(),
            "UTF8_STRING".to_string(),
        ];
        assert_eq!(
            preferred_clipboard_text_mime(&offered).as_deref(),
            Some("text/plain;charset=UTF-8")
        );
    }

    #[test]
    fn guest_image_offer_beats_browser_html_and_text() {
        let offered = vec!["text/html".into(), "text/plain".into(), "image/png".into()];
        assert_eq!(
            preferred_clipboard_mime(&offered).as_deref(),
            Some("image/png")
        );
        assert!(preferred_clipboard_mime(&["text/html".into()]).is_none());
    }

    fn read_test_offer(bytes: Vec<u8>, mime: &str) -> Result<Option<ClipboardPayload>, String> {
        use std::io::Write;
        let (reader, mut writer) = std::os::unix::net::UnixStream::pair().unwrap();
        let producer = std::thread::spawn(move || {
            let _ = writer.write_all(&bytes);
        });
        let result = read_guest_clipboard(reader.into(), mime, &super::AtomicU64::new(1), 1);
        producer.join().unwrap();
        result
    }

    #[test]
    fn guest_png_transfer_preserves_binary_bytes() {
        let png = include_bytes!("../assets/icon.png");
        let Some(ClipboardPayload::Image(received)) =
            read_test_offer(png.to_vec(), "image/png").unwrap()
        else {
            panic!("PNG was not received as an image");
        };
        assert_eq!(&*received, png);
    }

    #[test]
    fn guest_text_transfer_preserves_unicode_and_newlines() {
        let text = "clipboard \u{4f60}\u{597d}\n";
        let Some(ClipboardPayload::Text(received)) =
            read_test_offer(text.as_bytes().to_vec(), "text/plain").unwrap()
        else {
            panic!("Text was not received");
        };
        assert_eq!(&*received, text);
    }

    #[test]
    fn guest_transfer_rejects_invalid_png_and_unbounded_text() {
        assert!(read_test_offer(b"<img src='example'>".to_vec(), "image/png").is_err());
        assert!(
            read_test_offer(vec![b'x'; 8 * 1024 * 1024 + 1], "text/plain")
                .err()
                .unwrap()
                .contains("transfer limit")
        );
    }

    #[test]
    fn new_selection_cancels_a_blocked_clipboard_reader() {
        use std::sync::{
            Arc,
            atomic::{AtomicU64, Ordering},
        };
        let (reader, _writer) = std::os::unix::net::UnixStream::pair().unwrap();
        let counter = Arc::new(AtomicU64::new(1));
        let worker_counter = counter.clone();
        let reader = std::thread::spawn(move || {
            read_guest_clipboard(reader.into(), "text/plain", &worker_counter, 1)
        });
        counter.store(2, Ordering::Relaxed);
        assert!(reader.join().unwrap().unwrap().is_none());
    }

    #[test]
    fn clipboard_writer_handles_backpressure_without_truncating() {
        use std::io::Read;
        let (mut reader, writer) = std::os::unix::net::UnixStream::pair().unwrap();
        writer.set_nonblocking(true).unwrap();
        let bytes = vec![0xA5; 2 * 1024 * 1024];
        let expected = bytes.clone();
        let producer =
            std::thread::spawn(move || super::write_clipboard_payload(writer.into(), &bytes));
        // Let the sender fill the socket before beginning to consume it.
        std::thread::sleep(std::time::Duration::from_millis(30));
        let mut received = Vec::new();
        reader.read_to_end(&mut received).unwrap();
        producer.join().unwrap().unwrap();
        assert_eq!(received, expected);
    }

    #[test]
    fn clipboard_writer_releases_a_disconnected_receiver_without_waiting_for_timeout() {
        let (receiver, sender) = std::os::unix::net::UnixStream::pair().unwrap();
        drop(receiver);
        let (completed, result) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .spawn(move || {
                let error =
                    super::write_clipboard_payload(sender.into(), b"clipboard").unwrap_err();
                completed.send(error.kind()).unwrap();
            })
            .unwrap();
        assert_eq!(
            result
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap(),
            std::io::ErrorKind::BrokenPipe
        );
    }

    #[test]
    fn accepts_wayland_and_xwayland_text_aliases() {
        for mime in [
            "text/plain",
            "text/plain;charset=utf-8",
            "UTF8_STRING",
            "TEXT",
            "STRING",
        ] {
            assert!(is_clipboard_text_mime(mime), "rejected {mime}");
        }
        assert!(!is_clipboard_text_mime("image/png"));
    }
}

fn write_to_pasteboard(text: &str) -> isize {
    use objc2_app_kit::NSPasteboard;
    use objc2_foundation::NSString;
    unsafe {
        let pb = NSPasteboard::generalPasteboard();
        pb.clearContents();
        let ns_str = NSString::from_str(text);
        let pb_type = objc2_app_kit::NSPasteboardTypeString;
        pb.setString_forType(&ns_str, pb_type);
        pb.changeCount()
    }
}

fn pasteboard_snapshot_if_changed(previous_change_count: isize) -> Option<HostPasteboardSnapshot> {
    use objc2_app_kit::NSPasteboard;
    unsafe {
        let pb = NSPasteboard::generalPasteboard();
        let change_count = pb.changeCount();
        if change_count == previous_change_count {
            return None;
        }
        let text = pb
            .stringForType(objc2_app_kit::NSPasteboardTypeString)
            .map(|text| Arc::<str>::from(text.to_string()));
        let (image_png, image_tiff, image_error) = match copy_pasteboard_image(&pb) {
            Ok(Some(CopiedPasteboardImage::Png(image))) => (Some(image), None, None),
            Ok(Some(CopiedPasteboardImage::Tiff(image))) => (None, Some(image), None),
            Ok(None) => (None, None, None),
            Err(error) => (None, None, Some(error)),
        };
        // Provider fetching may block, and the owner may change while it does.
        // Never publish text and image fetched across different host snapshots.
        if pb.changeCount() != change_count {
            return None;
        }
        Some(HostPasteboardSnapshot {
            change_count,
            text,
            image_png,
            image_tiff,
            image_error,
        })
    }
}

fn copy_pasteboard_image(
    pb: &objc2_app_kit::NSPasteboard,
) -> Result<Option<CopiedPasteboardImage>, String> {
    use objc2_app_kit::{NSPasteboardTypePNG, NSPasteboardTypeTIFF};
    unsafe {
        if let Some(data) = pb.dataForType(NSPasteboardTypePNG) {
            return shared_png_data(&data).map(|image| image.map(CopiedPasteboardImage::Png));
        }
        let Some(tiff) = pb.dataForType(NSPasteboardTypeTIFF) else {
            return Ok(None);
        };
        let bytes = tiff.bytes();
        validate_clipboard_source_size(bytes.len())?;
        // Copy before leaving the event thread; even retained NSData may have a
        // mutable provider backing. Size is checked before allocating our copy.
        Ok(Some(CopiedPasteboardImage::Tiff(bytes.into())))
    }
}

fn convert_host_clipboard_tiff(tiff: Box<[u8]>) -> crate::clipboard_transfer::HostImageResult {
    use objc2_app_kit::{NSBitmapImageRep, NSPNGFileType};
    use objc2_foundation::{NSData, NSDictionary};

    validate_clipboard_source_size(tiff.len())?;
    // Reject dimension/depth bombs before asking AppKit to allocate a bitmap.
    validate_clipboard_tiff(&tiff)?;
    objc2::rc::autoreleasepool(|_| unsafe {
        let data = NSData::with_bytes(&tiff);
        let image = NSBitmapImageRep::imageRepWithData(&data)
            .ok_or_else(|| "macOS clipboard TIFF image could not be decoded".to_string())?;
        validate_clipboard_image_dimensions(image.pixelsWide(), image.pixelsHigh())?;
        let properties = NSDictionary::new();
        let png = image
            .representationUsingType_properties(NSPNGFileType, &properties)
            .ok_or_else(|| {
                "macOS clipboard TIFF image could not be converted to PNG".to_string()
            })?;
        let image = shared_png_data(&png)?
            .ok_or_else(|| "macOS clipboard TIFF conversion produced an empty PNG".to_string())?;
        Ok(Some(image))
    })
}

fn validate_clipboard_tiff(bytes: &[u8]) -> Result<(), String> {
    let malformed = || "macOS clipboard TIFF has malformed or unsupported metadata".to_string();
    let little = match bytes.get(..4) {
        Some(b"II\x2a\0") => true,
        Some(b"MM\0\x2a") => false,
        _ => return Err(malformed()), // BigTIFF and other formats are not admitted.
    };
    let u16_at = |offset: usize| -> Option<u16> {
        let value = bytes.get(offset..offset.checked_add(2)?)?.try_into().ok()?;
        Some(if little {
            u16::from_le_bytes(value)
        } else {
            u16::from_be_bytes(value)
        })
    };
    let u32_at = |offset: usize| -> Option<u32> {
        let value = bytes.get(offset..offset.checked_add(4)?)?.try_into().ok()?;
        Some(if little {
            u32::from_le_bytes(value)
        } else {
            u32::from_be_bytes(value)
        })
    };
    let ifd = u32_at(4).ok_or_else(malformed)? as usize;
    if ifd < 8 {
        return Err(malformed());
    }
    let count = u16_at(ifd).ok_or_else(malformed)? as usize;
    let start = ifd.checked_add(2).ok_or_else(malformed)?;
    let end = start
        .checked_add(count.checked_mul(12).ok_or_else(malformed)?)
        .ok_or_else(malformed)?;
    if u32_at(end).ok_or_else(malformed)? != 0 {
        return Err("macOS clipboard multi-image TIFF is not supported".into());
    }
    let mut width = None;
    let mut height = None;
    let mut samples = None;
    let mut depths = None;
    for entry in (start..end).step_by(12) {
        let tag = u16_at(entry).ok_or_else(malformed)?;
        if tag == 330 {
            return Err("macOS clipboard TIFF sub-images are not supported".into());
        }
        if !matches!(tag, 256 | 257 | 258 | 277) {
            continue;
        }
        let size = match u16_at(entry + 2).ok_or_else(malformed)? {
            3 => 2,
            4 => 4,
            _ => return Err(malformed()),
        };
        let length = u32_at(entry + 4).ok_or_else(malformed)? as usize;
        if length == 0 || length > 4 || (tag != 258 && length != 1) {
            return Err(malformed());
        }
        let offset = if length * size <= 4 {
            entry + 8
        } else {
            u32_at(entry + 8).ok_or_else(malformed)? as usize
        };
        let mut values = Vec::with_capacity(length);
        for index in 0..length {
            let offset = offset.checked_add(index * size).ok_or_else(malformed)?;
            let value = if size == 2 {
                u16_at(offset).map(u32::from)
            } else {
                u32_at(offset)
            };
            values.push(value.ok_or_else(malformed)?);
        }
        match tag {
            256 => {
                if width.replace(values[0]).is_some() {
                    return Err(malformed());
                }
            }
            257 => {
                if height.replace(values[0]).is_some() {
                    return Err(malformed());
                }
            }
            258 => {
                if depths.replace(values).is_some() {
                    return Err(malformed());
                }
            }
            277 => {
                if samples.replace(values[0]).is_some() {
                    return Err(malformed());
                }
            }
            _ => {}
        }
    }
    let samples = samples.unwrap_or(1);
    if samples == 0
        || samples > 4
        || depths.as_ref().is_some_and(|depths| {
            (depths.len() != 1 && depths.len() != samples as usize)
                || depths.iter().any(|bits| *bits == 0 || *bits > 16)
        })
    {
        return Err("macOS clipboard TIFF exceeds the 4-channel, 16-bit decode limit".into());
    }
    validate_clipboard_image_dimensions(
        width.ok_or_else(malformed)? as isize,
        height.ok_or_else(malformed)? as isize,
    )
}

fn shared_png_data(data: &objc2_foundation::NSData) -> Result<Option<Arc<[u8]>>, String> {
    let bytes = data.bytes();
    if bytes.len() > MAX_CLIPBOARD_IMAGE_BYTES {
        return Err(format!(
            "macOS clipboard image is larger than the 64 MiB transfer limit ({} MiB)",
            bytes.len().div_ceil(1024 * 1024)
        ));
    }
    shared_png_bytes(bytes.to_vec())
}

fn shared_png_bytes(bytes: Vec<u8>) -> Result<Option<Arc<[u8]>>, String> {
    if bytes.is_empty() {
        return Ok(None);
    }
    if bytes.len() > MAX_CLIPBOARD_IMAGE_BYTES {
        return Err(format!(
            "macOS clipboard image is larger than the 64 MiB transfer limit ({} MiB)",
            bytes.len().div_ceil(1024 * 1024)
        ));
    }
    let (width, height) = png_dimensions(&bytes)
        .ok_or_else(|| "macOS clipboard advertised malformed PNG data".to_string())?;
    validate_clipboard_image_dimensions(width as isize, height as isize)?;
    Ok(Some(Arc::from(bytes)))
}

fn validate_clipboard_source_size(bytes: usize) -> Result<(), String> {
    if bytes > MAX_CLIPBOARD_IMAGE_SOURCE_BYTES {
        return Err(format!(
            "macOS clipboard source image is larger than the 128 MiB decode limit ({} MiB)",
            bytes.div_ceil(1024 * 1024)
        ));
    }
    Ok(())
}

fn validate_clipboard_image_dimensions(width: isize, height: isize) -> Result<(), String> {
    if width <= 0 || height <= 0 {
        return Err(format!(
            "macOS clipboard image has invalid dimensions {width}x{height}"
        ));
    }
    let pixels = (width as usize)
        .checked_mul(height as usize)
        .ok_or_else(|| "macOS clipboard image dimensions overflow".to_string())?;
    if pixels > MAX_CLIPBOARD_IMAGE_PIXELS {
        return Err(format!(
            "macOS clipboard image exceeds the 32-megapixel safety limit ({width}x{height})"
        ));
    }
    Ok(())
}

fn png_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
    if bytes.len() < 24 || &bytes[..8] != PNG_SIGNATURE || &bytes[12..16] != b"IHDR" {
        return None;
    }
    let width = u32::from_be_bytes(bytes[16..20].try_into().ok()?);
    let height = u32::from_be_bytes(bytes[20..24].try_into().ok()?);
    Some((width, height))
}
