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

const CLIPBOARD_IMAGE_MIME: &str = "image/png";
const MAX_CLIPBOARD_IMAGE_BYTES: usize = 64 * 1024 * 1024;
const MAX_CLIPBOARD_IMAGE_SOURCE_BYTES: usize = 128 * 1024 * 1024;
const MAX_CLIPBOARD_IMAGE_PIXELS: usize = 32 * 1024 * 1024;

struct HostPasteboardSnapshot {
    change_count: isize,
    text: Option<Arc<str>>,
    image_png: Option<Arc<[u8]>>,
    image_error: Option<String>,
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
    pub loop_signal: std::sync::mpsc::Sender<crate::messages::CompositorMessage>,
    pub presentation: crate::presentation::PresentationMode,
    pub width: u32,
    pub height: u32,
    pub scale_factor: f64,
    /// Monotonic start time — used to compute frame timestamps for wl_callback::done.
    pub start_time: std::time::Instant,
    /// Frame callbacks collected during commit(); fired after swap_buffers().
    pub pending_frame_callbacks: Vec<PendingFrameCallback>,
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
    host_clipboard_text: Option<Arc<str>>,
    host_clipboard_image_png: Option<Arc<[u8]>>,
    pending_guest_clipboard_mime: Option<String>,
    guest_clipboard_generation: Arc<AtomicU64>,
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
                ((-logical.x, -logical.y), AxisSource::Finger, None)
            }
        };

        let horizontal_moved = axis.0 != 0.0;
        let vertical_moved = axis.1 != 0.0;
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
        loop_signal: std::sync::mpsc::Sender<crate::messages::CompositorMessage>,
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
            needs_redraw: true,
            rootless_dirty_surfaces: std::collections::HashSet::new(),
            commit_counter: 0,
            host_clipboard_text: None,
            host_clipboard_image_png: None,
            pending_guest_clipboard_mime: None,
            guest_clipboard_generation: Arc::new(AtomicU64::new(0)),
            pasteboard_change_count: -1,
            last_pasteboard_poll: std::time::Instant::now() - std::time::Duration::from_millis(100),
            pointer_gesture: PointerGestureTracker::default(),
            pointer_axis: PointerAxisTracker::default(),
        })
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

    fn root_toplevel_id_for_surface(
        &self,
        surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
    ) -> Option<smithay::reexports::wayland_server::backend::ObjectId> {
        let mut root = surface.clone();
        loop {
            while let Some(parent) = smithay::wayland::compositor::get_parent(&root) {
                root = parent;
            }
            let Some(parent) = self
                .popups
                .iter()
                .find(|popup| popup.wl_surface() == &root)
                .and_then(|popup| popup.get_parent_surface())
            else {
                break;
            };
            root = parent;
        }
        self.toplevels
            .iter()
            .find(|toplevel| toplevel.wl_surface() == &root)
            .map(|toplevel| toplevel.wl_surface().id())
    }

    pub fn take_frame_callbacks_for(
        &mut self,
        root_surface: Option<&smithay::reexports::wayland_server::backend::ObjectId>,
    ) -> Vec<smithay::reexports::wayland_server::protocol::wl_callback::WlCallback> {
        if root_surface.is_none() {
            return std::mem::take(&mut self.pending_frame_callbacks)
                .into_iter()
                .map(|pending| pending.callback)
                .collect();
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
                selected.push(pending.callback);
            } else {
                remaining.push(pending);
            }
        }
        self.pending_frame_callbacks = remaining;
        selected
    }

    pub fn poll_host_clipboard(&mut self) {
        let now = std::time::Instant::now();
        if now.duration_since(self.last_pasteboard_poll) < std::time::Duration::from_millis(100) {
            return;
        }
        self.last_pasteboard_poll = now;

        let Some(snapshot) = pasteboard_snapshot_if_changed(self.pasteboard_change_count) else {
            return;
        };
        self.guest_clipboard_generation
            .fetch_add(1, Ordering::Relaxed);
        self.pending_guest_clipboard_mime = None;
        self.pasteboard_change_count = snapshot.change_count;
        self.host_clipboard_text = snapshot.text;
        self.host_clipboard_image_png = snapshot.image_png;

        let byte_count = self
            .host_clipboard_text
            .as_ref()
            .map_or(0, |text| text.len())
            + self
                .host_clipboard_image_png
                .as_ref()
                .map_or(0, |image| image.len());
        let mime_types = clipboard_mime_types(
            self.host_clipboard_text.is_some(),
            self.host_clipboard_image_png.is_some(),
        );
        crate::diagnostics::record_clipboard_host_change(
            byte_count,
            mime_types.first().map(String::as_str),
        );

        if let Some(error) = snapshot.image_error {
            log::warn!("Clipboard: {error}");
            crate::diagnostics::record_clipboard_host_failure(error);
        }

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
            (),
        );
    }

    pub fn install_guest_clipboard(
        &mut self,
        generation: u64,
        pasteboard_change_count: isize,
        payload: ClipboardPayload,
    ) {
        // A slow source must not replace a newer guest selection or host copy.
        if self.guest_clipboard_generation.load(Ordering::Relaxed) != generation
            || unsafe { objc2_app_kit::NSPasteboard::generalPasteboard().changeCount() }
                != pasteboard_change_count
        {
            return;
        }
        let byte_count = payload.as_bytes().len();
        match payload {
            ClipboardPayload::Text(text) => {
                if self.host_clipboard_text.as_deref() == Some(&*text)
                    && self.host_clipboard_image_png.is_none()
                {
                    return;
                }
                self.pasteboard_change_count = write_to_pasteboard(&text);
                self.host_clipboard_text = Some(text);
                self.host_clipboard_image_png = None;
            }
            ClipboardPayload::Image(image) => {
                if self.host_clipboard_image_png.as_deref() == Some(&*image)
                    && self.host_clipboard_text.is_none()
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
                self.host_clipboard_text = None;
                self.host_clipboard_image_png = Some(image);
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
                self.host_clipboard_text.is_some(),
                self.host_clipboard_image_png.is_some(),
            ),
            (),
        );
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
        std::thread::spawn(move || {
            let result = read_guest_clipboard(read_fd, &mime, &generation_counter, generation);
            match result {
                Ok(Some(payload)) => {
                    let _ = loop_signal.send(crate::messages::CompositorMessage::GuestClipboard {
                        generation,
                        pasteboard_change_count,
                        payload,
                    });
                }
                Ok(None) => {}
                Err(error) => crate::diagnostics::record_clipboard_failure(error),
            }
        });
    }
    pub fn update_scale_factor(&mut self, scale: f64) {
        let scale = if scale.is_finite() && scale > 0.0 {
            scale
        } else {
            log::warn!("Ignoring invalid window scale factor: {}", scale);
            1.0
        };
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
        use smithay::wayland::compositor::{
            SurfaceAttributes, TraversalAction, with_surface_tree_downward,
        };
        // Resolve the owner before walking the tree. Smithay holds its surface-tree
        // mutex during traversal, so calling get_parent from the visitor deadlocks.
        let root_surface = self.root_toplevel_id_for_surface(surface);
        let mut new_cbs = Vec::new();
        with_surface_tree_downward(
            surface,
            (),
            |_, _, _| TraversalAction::DoChildren(()),
            |callback_surface, states, _| {
                let mut guard = states.cached_state.get::<SurfaceAttributes>();
                new_cbs.extend(guard.current().frame_callbacks.drain(..).map(|callback| {
                    PendingFrameCallback {
                        root_surface: root_surface.clone(),
                        source_surface: callback_surface.clone(),
                        callback,
                    }
                }));
            },
            |_, _, _| true,
        );
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
        let surface_id = surface.id();
        let was_toplevel = self
            .toplevels
            .iter()
            .any(|toplevel| toplevel.wl_surface() == surface);
        self.layout.remove_tile(&surface_id);
        self.toplevels
            .retain(|toplevel| toplevel.wl_surface() != surface);
        self.popups.retain(|popup| popup.wl_surface() != surface);
        self.surface_positions.remove(&surface_id);
        self.pending_frame_callbacks
            .retain(|pending| pending.source_surface != *surface);
        if was_toplevel {
            self.output.leave(surface);
            self.rootless_dirty_surfaces.remove(&surface_id);
            self.pending_frame_callbacks
                .retain(|pending| pending.root_surface.as_ref() != Some(&surface_id));
        }
        if self
            .drag_state
            .as_ref()
            .is_some_and(|(id, _)| id == &surface_id)
        {
            self.drag_state = None;
        }
        if self.start_drag_request.as_ref() == Some(&surface_id) {
            self.start_drag_request = None;
        }

        if let Some(keyboard) = self.seat.get_keyboard() {
            if keyboard.current_focus().as_ref() == Some(surface) {
                keyboard.set_focus(self, None, smithay::utils::SERIAL_COUNTER.next_serial());
            }
        }
        self.needs_redraw = true;
        if self.presentation.is_rootless() {
            let _ = self.loop_signal.send(
                crate::messages::CompositorMessage::RootlessSurfaceDestroyed(surface_id.clone()),
            );
        }
        if was_toplevel && self.presentation.is_rootless() {
            let _ = self.loop_signal.send(
                crate::messages::CompositorMessage::RootlessToplevelDestroyed(surface_id.clone()),
            );
        }
        log::info!("Removed destroyed surface {surface_id:?} from the compositor layout");
    }
}
impl XdgShellHandler for AppState {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }
    fn new_toplevel(&mut self, surface: smithay::wayland::shell::xdg::ToplevelSurface) {
        log::info!("New XDG Toplevel Created: {:?}", surface.wl_surface().id());
        if !self.toplevels.contains(&surface) {
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
        let geo = positioner.get_geometry();
        surface.with_pending_state(|state| {
            state.geometry = geo;
        });
        if surface.send_configure().is_err() {
            return;
        }
        self.popups.push(surface);
        self.needs_redraw = true;
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
        use objc2_app_kit::NSCursor;
        use smithay::input::pointer::CursorIcon;
        unsafe {
            match image {
                CursorImageStatus::Hidden => NSCursor::hide(),
                CursorImageStatus::Named(icon) => {
                    let cursor = match icon {
                        CursorIcon::Text | CursorIcon::VerticalText => NSCursor::IBeamCursor(),
                        CursorIcon::Pointer => NSCursor::pointingHandCursor(),
                        CursorIcon::Move | CursorIcon::AllScroll => NSCursor::openHandCursor(),
                        CursorIcon::Grab => NSCursor::openHandCursor(),
                        CursorIcon::Grabbing => NSCursor::closedHandCursor(),
                        CursorIcon::Crosshair => NSCursor::crosshairCursor(),
                        CursorIcon::NotAllowed | CursorIcon::NoDrop => {
                            NSCursor::operationNotAllowedCursor()
                        }
                        CursorIcon::EResize
                        | CursorIcon::WResize
                        | CursorIcon::EwResize
                        | CursorIcon::ColResize => NSCursor::resizeLeftRightCursor(),
                        CursorIcon::NResize
                        | CursorIcon::SResize
                        | CursorIcon::NsResize
                        | CursorIcon::RowResize => NSCursor::resizeUpDownCursor(),
                        CursorIcon::NeResize | CursorIcon::SwResize | CursorIcon::NeswResize => {
                            NSCursor::resizeLeftRightCursor()
                        }
                        CursorIcon::NwResize | CursorIcon::SeResize | CursorIcon::NwseResize => {
                            NSCursor::resizeLeftRightCursor()
                        }
                        CursorIcon::Copy => NSCursor::dragCopyCursor(),
                        CursorIcon::Alias => NSCursor::dragLinkCursor(),
                        CursorIcon::ContextMenu => NSCursor::contextualMenuCursor(),
                        CursorIcon::ZoomIn | CursorIcon::ZoomOut => NSCursor::crosshairCursor(),
                        _ => NSCursor::arrowCursor(),
                    };
                    cursor.set();
                }
                CursorImageStatus::Surface(_) => {
                    // Custom surface cursor — use arrow fallback for now
                    NSCursor::arrowCursor().set();
                }
            }
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
    type SelectionUserData = ();

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
        _user_data: &Self::SelectionUserData,
    ) {
        if ty != SelectionTarget::Clipboard {
            return;
        }
        let payload = if is_clipboard_image_mime(&mime_type) {
            self.host_clipboard_image_png
                .clone()
                .map(ClipboardPayload::Image)
        } else if is_clipboard_text_mime(&mime_type) {
            self.host_clipboard_text.clone().map(ClipboardPayload::Text)
        } else {
            return;
        };
        log::info!("Clipboard: Wayland client requested macOS contents as {mime_type}");
        std::thread::spawn(move || {
            if let Some(payload) = payload {
                if let Err(error) = write_clipboard_payload(fd, payload.as_bytes()) {
                    crate::diagnostics::record_clipboard_host_failure(format!(
                        "Failed to send macOS clipboard contents to Wayland: {error}"
                    ));
                }
            }
        });
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
        _surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
        _pointer: &smithay::input::pointer::PointerHandle<Self>,
    ) {
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
        assert_eq!(frame.axis, (-6.0, 4.0));
        assert_eq!(frame.stop, (false, false));
        assert_eq!(frame.v120, None);
    }

    #[test]
    fn zero_delta_end_stops_every_active_trackpad_axis() {
        let mut tracker = PointerAxisTracker::default();
        tracker.frame(
            1.0,
            MouseScrollDelta::PixelDelta(PhysicalPosition::new(3.0, 5.0)),
            TouchPhase::Started,
            10,
        );
        let end = tracker
            .frame(
                1.0,
                MouseScrollDelta::PixelDelta(PhysicalPosition::new(0.0, 0.0)),
                TouchPhase::Ended,
                11,
            )
            .expect("the terminal frame must carry axis_stop");

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
                .is_none(),
            "a completed gesture must not leak active axes"
        );
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
        let (image_png, image_error) = match pasteboard_png(&pb) {
            Ok(image) => (image, None),
            Err(error) => (None, Some(error)),
        };
        Some(HostPasteboardSnapshot {
            change_count,
            text,
            image_png,
            image_error,
        })
    }
}

fn pasteboard_png(pb: &objc2_app_kit::NSPasteboard) -> Result<Option<Arc<[u8]>>, String> {
    use objc2_app_kit::{
        NSBitmapImageRep, NSPNGFileType, NSPasteboardTypePNG, NSPasteboardTypeTIFF,
    };
    use objc2_foundation::NSDictionary;

    unsafe {
        if let Some(data) = pb.dataForType(NSPasteboardTypePNG) {
            return shared_png_data(&data);
        }

        let Some(tiff) = pb.dataForType(NSPasteboardTypeTIFF) else {
            return Ok(None);
        };
        validate_clipboard_source_size(tiff.len())?;
        let image = NSBitmapImageRep::imageRepWithData(&tiff)
            .ok_or_else(|| "macOS clipboard TIFF image could not be decoded".to_string())?;
        validate_clipboard_image_dimensions(image.pixelsWide(), image.pixelsHigh())?;
        let properties = NSDictionary::new();
        let png = image
            .representationUsingType_properties(NSPNGFileType, &properties)
            .ok_or_else(|| {
                "macOS clipboard TIFF image could not be converted to PNG".to_string()
            })?;
        shared_png_data(&png)
    }
}

fn shared_png_data(data: &objc2_foundation::NSData) -> Result<Option<Arc<[u8]>>, String> {
    if data.len() > MAX_CLIPBOARD_IMAGE_BYTES {
        return Err(format!(
            "macOS clipboard image is larger than the 64 MiB transfer limit ({} MiB)",
            data.len().div_ceil(1024 * 1024)
        ));
    }
    shared_png_bytes(data.bytes().to_vec())
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
