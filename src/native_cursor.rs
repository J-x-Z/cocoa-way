//! AppKit owns pointer presentation; cursor commits never need a scene redraw.
//!
//! Call `entered`/`left` for the native content view, and `refresh` after Wayland
//! dispatch or constraint transitions. All methods run on the AppKit main thread.
//! A nested compositor's cursor baked into its window buffer cannot be extracted
//! here: it must export a cursor surface/shape and stop baking it in. Its explicit
//! Hidden request still hides the host cursor, but does not reduce baked-in latency.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};

use objc2::rc::Retained;
use objc2_app_kit::{
    NSBitmapFormat, NSBitmapImageRep, NSCursor, NSDeviceRGBColorSpace, NSImage, NSView, NSWindow,
};
use objc2_foundation::{MainThreadMarker, NSPoint, NSSize};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use smithay::input::pointer::{CursorIcon, CursorImageStatus, CursorImageSurfaceData};
use smithay::reexports::wayland_server::{
    Resource,
    backend::ObjectId,
    protocol::{
        wl_buffer::WlBuffer, wl_callback::WlCallback, wl_output::Transform, wl_shm::Format,
        wl_surface::WlSurface,
    },
};
use smithay::wayland::{
    compositor::{BufferAssignment, SurfaceAttributes, get_role, with_states},
    seat::CURSOR_IMAGE_ROLE,
    shm::with_buffer_contents,
    viewporter::ViewportCachedState,
};
use winit::window::{Window, WindowId};

const MAX_CURSOR_DIMENSION: i32 = 512;

struct CursorVisibility(AtomicBool);

impl CursorVisibility {
    fn update(&self, hidden: bool) -> Option<bool> {
        (self.0.swap(hidden, Ordering::Relaxed) != hidden).then_some(hidden)
    }
}

static VISIBILITY: CursorVisibility = CursorVisibility(AtomicBool::new(false));

fn set_hidden(hidden: bool) {
    if let Some(hidden) = VISIBILITY.update(hidden) {
        // NSCursor hide/unhide are counted, unlike winit's invisible cursor rect.
        unsafe {
            if hidden {
                NSCursor::hide();
            } else {
                NSCursor::unhide();
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Geometry {
    scale: i32,
    transform: Transform,
    destination: Option<(i32, i32)>,
    cropped: bool,
}

impl Default for Geometry {
    fn default() -> Self {
        Self {
            scale: 1,
            transform: Transform::Normal,
            destination: None,
            cropped: false,
        }
    }
}

#[derive(Default)]
struct SurfaceCursor {
    // Keep the resource for damage-only commits, but never reread it on motion
    // or hotspot-only requests. Each new attachment is released after copying.
    buffer: Option<WlBuffer>,
    bitmap: Option<Retained<NSBitmapImageRep>>,
    cursor: Option<Retained<NSCursor>>,
    geometry: Geometry,
    hotspot: (i32, i32),
    revision: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Applied {
    Hidden,
    Named(CursorIcon),
    Surface(ObjectId, u64),
}

struct CursorFocus<T> {
    current: Option<T>,
    requested: Option<T>,
}

impl<T> Default for CursorFocus<T> {
    fn default() -> Self {
        Self {
            current: None,
            requested: None,
        }
    }
}

impl<T: Clone + Eq> CursorFocus<T> {
    fn update(&mut self, focus: Option<T>) -> bool {
        let reset = focus.is_some() && focus != self.requested;
        if reset {
            self.requested = focus.clone();
        }
        self.current = focus;
        reset
    }

    fn request(&mut self) {
        self.requested = self.current.clone();
    }

    fn valid(&self) -> bool {
        self.current.is_some() && self.current == self.requested
    }
}

pub(super) struct NativeCursor {
    requested: CursorImageStatus,
    surfaces: HashMap<ObjectId, SurfaceCursor>,
    // While inside content, prevent winit resetCursorRects from overriding our
    // native shape. Restore AppKit's titlebar/resize cursors on content leave.
    window: Option<Retained<NSWindow>>,
    window_id: Option<WindowId>,
    applied: Option<Applied>,
    locked: bool,
    focus: CursorFocus<ObjectId>,
    points_per_logical: f64,
}

impl Default for NativeCursor {
    fn default() -> Self {
        Self {
            requested: CursorImageStatus::default_named(),
            surfaces: HashMap::new(),
            window: None,
            window_id: None,
            applied: None,
            locked: false,
            focus: CursorFocus::default(),
            points_per_logical: 1.0,
        }
    }
}

impl NativeCursor {
    pub(super) fn diagnostics(&self) -> serde_json::Value {
        let requested = match &self.requested {
            CursorImageStatus::Hidden => "hidden",
            CursorImageStatus::Named(_) => "named",
            CursorImageStatus::Surface(_) => "surface",
        };
        let presentation = if self.window.is_none() {
            "outside_content"
        } else if self.locked {
            "pointer_locked"
        } else {
            match &self.applied {
                Some(Applied::Hidden) => "client_hidden",
                Some(Applied::Named(_)) => "native_shape",
                Some(Applied::Surface(_, _)) => "native_bitmap",
                None => "pending",
            }
        };
        let native_visible = self.window.is_some()
            && matches!(
                self.applied,
                Some(Applied::Named(_) | Applied::Surface(_, _))
            );
        serde_json::json!({
            "requested": requested,
            "presentation": presentation,
            "position_source": if native_visible { "macOS" } else { "no_visible_native_cursor" },
            "native_cursor_visible": native_visible,
            "motion_waits_for_guest_frame": native_visible.then_some(false),
            "client_requested_hidden": matches!(self.requested, CursorImageStatus::Hidden),
            "pointer_locked": self.locked,
            "logical_to_points": self.points_per_logical,
            "note": "A cursor baked into guest scene pixels is not a Wayland cursor surface; client_hidden does not identify why the client hides it.",
        })
    }

    pub(super) fn entered(&mut self, window: &Window) {
        self.entered_with_id(window, window.id());
    }

    pub(super) fn ensure_entered(&mut self, window: &Window, id: WindowId) -> bool {
        // Use the ID already present in WindowEvent: Window::id() itself can
        // perform winit's main-thread checks on macOS.
        if self.window_id == Some(id) {
            return false;
        }
        self.entered_with_id(window, id);
        self.window.is_some()
    }

    fn entered_with_id(&mut self, window: &Window, id: WindowId) {
        self.left();
        let Ok(handle) = window.window_handle() else {
            return;
        };
        let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
            return;
        };
        // Winit guarantees ns_view is live for the borrowed window handle.
        let view = unsafe { &*handle.ns_view.as_ptr().cast::<NSView>() };
        self.window = view.window();
        if let Some(window) = &self.window {
            unsafe { window.disableCursorRects() };
            self.window_id = Some(id);
        }
    }

    pub(super) fn left(&mut self) {
        set_hidden(false);
        self.applied = None;
        self.locked = false;
        self.window_id = None;
        if let Some(window) = self.window.take() {
            unsafe { window.enableCursorRects() };
        }
    }

    pub(super) fn left_window(&mut self, id: WindowId) {
        // Focus loss/removal for another rootless window must not unhide the
        // cursor in the window currently beneath the pointer.
        if self.window_id == Some(id) {
            self.left();
        }
    }

    pub(super) fn set_locked(&mut self, locked: bool) {
        self.locked = locked;
    }

    pub(super) fn set_focus(&mut self, focus: Option<ObjectId>) {
        // A native leave preserves the old request, not its authority over a
        // different surface. No focus presents an arrow until reentry is known.
        if self.focus.update(focus) {
            self.requested = CursorImageStatus::default_named();
        }
    }

    pub(super) fn set_point_scale(&mut self, scale: f64) {
        if !scale.is_finite() || scale <= 0.0 || self.points_per_logical == scale {
            return;
        }
        self.points_per_logical = scale;
        for cached in self.surfaces.values_mut() {
            cached.cursor = make_surface_cursor(cached, scale);
            cached.revision = cached.revision.wrapping_add(1);
        }
    }

    pub(super) fn set_image(&mut self, image: CursorImageStatus) {
        // Smithay sends default_named on pointer.motion(None). Preserve the
        // previous client request across a native leave/enter, including Hidden.
        if self.window.is_none() && image == CursorImageStatus::default_named() {
            return;
        }
        if let CursorImageStatus::Surface(surface) = &image {
            self.update_surface(surface);
        }
        self.requested = image;
        self.focus.request();
    }

    /// Returns callbacks only for cursor-role surfaces, including inactive cursors.
    /// The owner paces them on its cursor clock; never acknowledge in this path.
    pub(super) fn committed(&mut self, surface: &WlSurface) -> Option<Vec<WlCallback>> {
        if get_role(surface) != Some(CURSOR_IMAGE_ROLE) {
            return None;
        }
        self.update_surface(surface);
        self.refresh();
        let callbacks = with_states(surface, |states| {
            std::mem::take(
                &mut states
                    .cached_state
                    .get::<SurfaceAttributes>()
                    .current()
                    .frame_callbacks,
            )
        });
        Some(callbacks)
    }

    fn update_surface(&mut self, surface: &WlSurface) {
        let (assignment, damaged, geometry, hotspot) = with_states(surface, |states| {
            let mut attributes = states.cached_state.get::<SurfaceAttributes>();
            let current = attributes.current();
            let damaged = !current.damage.is_empty();
            current.damage.clear();
            let delta = current.buffer_delta.take();
            let hotspot = states
                .data_map
                .get::<CursorImageSurfaceData>()
                .map(|data| {
                    let mut data = data.lock().unwrap();
                    if let Some(delta) = delta {
                        data.hotspot.x = data.hotspot.x.saturating_sub(delta.x);
                        data.hotspot.y = data.hotspot.y.saturating_sub(delta.y);
                    }
                    (data.hotspot.x, data.hotspot.y)
                })
                .unwrap_or((0, 0));
            let mut viewport = states.cached_state.get::<ViewportCachedState>();
            let viewport = viewport.current();
            let geometry = Geometry {
                scale: current.buffer_scale.max(1),
                transform: current.buffer_transform,
                destination: viewport.dst.map(|size| (size.w, size.h)),
                // Arbitrary viewport source cropping is not a cursor fast path.
                cropped: viewport.src.is_some(),
            };
            (current.buffer.take(), damaged, geometry, hotspot)
        });

        let cached = self.surfaces.entry(surface.id()).or_default();
        let replaced = assignment.is_some();
        let new_buffer = match assignment {
            Some(BufferAssignment::NewBuffer(buffer)) => {
                cached.buffer = Some(buffer.clone());
                Some(buffer)
            }
            Some(BufferAssignment::Removed) => {
                cached.buffer = None;
                None
            }
            None => None,
        };
        let pixels_changed = replaced || damaged || cached.geometry.transform != geometry.transform;
        let geometry_changed = cached.geometry != geometry || cached.hotspot != hotspot;
        if pixels_changed {
            cached.bitmap = cached
                .buffer
                .as_ref()
                .and_then(|buffer| read_bitmap(buffer, geometry.transform));
        }
        cached.geometry = geometry;
        cached.hotspot = hotspot;
        if pixels_changed || geometry_changed {
            cached.cursor = make_surface_cursor(cached, self.points_per_logical);
            cached.revision = cached.revision.wrapping_add(1);
        }
        if let Some(buffer) = new_buffer {
            buffer.release();
        }
    }

    pub(super) fn destroyed(&mut self, surface: &WlSurface) -> bool {
        let was_cursor = self.surfaces.remove(&surface.id()).is_some();
        if matches!(&self.requested, CursorImageStatus::Surface(current) if current == surface) {
            self.requested = CursorImageStatus::default_named();
        }
        was_cursor
    }

    pub(super) fn refresh(&mut self) {
        if self.window.is_none() {
            return;
        }
        let valid_focus = self.focus.valid();
        let desired = if self.locked
            || (valid_focus && matches!(self.requested, CursorImageStatus::Hidden))
        {
            Applied::Hidden
        } else if !valid_focus {
            Applied::Named(CursorIcon::Default)
        } else {
            match &self.requested {
                CursorImageStatus::Named(icon) => Applied::Named(*icon),
                CursorImageStatus::Surface(surface) => match self.surfaces.get(&surface.id()) {
                    Some(cached) if cached.bitmap.is_none() && cached.buffer.is_none() => {
                        Applied::Hidden
                    }
                    Some(cached) if cached.cursor.is_some() => {
                        Applied::Surface(surface.id(), cached.revision)
                    }
                    _ => Applied::Named(CursorIcon::Default),
                },
                CursorImageStatus::Hidden => Applied::Hidden,
            }
        };
        let hidden = desired == Applied::Hidden;
        if self.applied.as_ref() == Some(&desired) && VISIBILITY.0.load(Ordering::Relaxed) == hidden
        {
            return;
        }
        if !hidden {
            let cursor = match &desired {
                Applied::Named(icon) => named_cursor(*icon),
                Applied::Surface(id, _) => self.surfaces[id].cursor.as_ref().unwrap().clone(),
                Applied::Hidden => unreachable!(),
            };
            unsafe { cursor.set() };
        }
        set_hidden(hidden);
        self.applied = Some(desired);
    }
}

impl Drop for NativeCursor {
    fn drop(&mut self) {
        self.left();
    }
}

fn named_cursor(icon: CursorIcon) -> Retained<NSCursor> {
    match icon {
        CursorIcon::Text => NSCursor::IBeamCursor(),
        CursorIcon::VerticalText => NSCursor::IBeamCursorForVerticalLayout(),
        CursorIcon::Pointer => NSCursor::pointingHandCursor(),
        CursorIcon::Move | CursorIcon::AllScroll | CursorIcon::AllResize | CursorIcon::Grab => {
            NSCursor::openHandCursor()
        }
        CursorIcon::Grabbing => NSCursor::closedHandCursor(),
        CursorIcon::Crosshair | CursorIcon::Cell | CursorIcon::ZoomIn | CursorIcon::ZoomOut => {
            NSCursor::crosshairCursor()
        }
        CursorIcon::NotAllowed | CursorIcon::NoDrop => NSCursor::operationNotAllowedCursor(),
        CursorIcon::EResize => NSCursor::resizeRightCursor(),
        CursorIcon::WResize => NSCursor::resizeLeftCursor(),
        CursorIcon::NResize => NSCursor::resizeUpCursor(),
        CursorIcon::SResize => NSCursor::resizeDownCursor(),
        CursorIcon::EwResize | CursorIcon::ColResize => NSCursor::resizeLeftRightCursor(),
        CursorIcon::NsResize | CursorIcon::RowResize => NSCursor::resizeUpDownCursor(),
        // AppKit 0.2's public API has no diagonal/zoom/wait/help cursor selectors.
        CursorIcon::NeResize
        | CursorIcon::SwResize
        | CursorIcon::NeswResize
        | CursorIcon::NwResize
        | CursorIcon::SeResize
        | CursorIcon::NwseResize => NSCursor::crosshairCursor(),
        CursorIcon::Copy => NSCursor::dragCopyCursor(),
        CursorIcon::Alias => NSCursor::dragLinkCursor(),
        CursorIcon::ContextMenu => NSCursor::contextualMenuCursor(),
        CursorIcon::Default
        | CursorIcon::Help
        | CursorIcon::Progress
        | CursorIcon::Wait
        | CursorIcon::DndAsk => NSCursor::arrowCursor(),
        _ => NSCursor::arrowCursor(),
    }
}

fn image_geometry(
    width: i32,
    height: i32,
    geometry: Geometry,
    hotspot: (i32, i32),
    point_scale: f64,
) -> Option<(NSSize, NSPoint)> {
    if width <= 0
        || height <= 0
        || geometry.scale <= 0
        || geometry.cropped
        || !point_scale.is_finite()
        || point_scale <= 0.0
    {
        return None;
    }
    let (width, height) = geometry
        .destination
        .map(|(w, h)| (f64::from(w), f64::from(h)))
        .unwrap_or((
            f64::from(width) / f64::from(geometry.scale),
            f64::from(height) / f64::from(geometry.scale),
        ));
    if width <= 0.0 || height <= 0.0 {
        return None;
    }
    // wl_pointer hotspot is already in surface logical coordinates, not pixels.
    let size = NSSize::new(width * point_scale, height * point_scale);
    let hotspot = NSPoint::new(
        f64::from(hotspot.0) * point_scale,
        f64::from(hotspot.1) * point_scale,
    );
    (size.width.is_finite()
        && size.height.is_finite()
        && hotspot.x.is_finite()
        && hotspot.y.is_finite())
    .then_some((size, hotspot))
}

fn make_surface_cursor(cached: &SurfaceCursor, point_scale: f64) -> Option<Retained<NSCursor>> {
    let bitmap = cached.bitmap.as_ref()?;
    let (size, hotspot) = image_geometry(
        unsafe { bitmap.pixelsWide() } as i32,
        unsafe { bitmap.pixelsHigh() } as i32,
        cached.geometry,
        cached.hotspot,
        point_scale,
    )?;
    // These methods are called only by the main-thread Wayland/winit loop.
    let mtm = unsafe { MainThreadMarker::new_unchecked() };
    let image = unsafe { NSImage::initWithSize(mtm.alloc(), size) };
    unsafe {
        bitmap.setSize(size);
        image.addRepresentation(bitmap);
    }
    Some(NSCursor::initWithImage_hotSpot(
        mtm.alloc(),
        &image,
        hotspot,
    ))
}

fn buffer_layout(
    len: usize,
    offset: i32,
    width: i32,
    height: i32,
    stride: i32,
) -> Option<(usize, usize)> {
    if offset < 0
        || width <= 0
        || height <= 0
        || width > MAX_CURSOR_DIMENSION
        || height > MAX_CURSOR_DIMENSION
        || stride <= 0
    {
        return None;
    }
    let row_bytes = (width as usize).checked_mul(4)?;
    let stride = stride as usize;
    if stride < row_bytes {
        return None;
    }
    let end = (offset as usize)
        .checked_add((height as usize - 1).checked_mul(stride)?)?
        .checked_add(row_bytes)?;
    (end <= len).then_some((offset as usize, stride))
}

fn pixel_bgra(format: Format, bytes: &[u8]) -> Option<[u8; 4]> {
    let [a, b, c, d]: [u8; 4] = bytes.try_into().ok()?;
    match format {
        Format::Argb8888 => Some([a, b, c, d]),
        Format::Xrgb8888 => Some([a, b, c, 255]),
        Format::Abgr8888 => Some([c, b, a, d]),
        Format::Xbgr8888 => Some([c, b, a, 255]),
        Format::Rgba8888 => Some([b, c, d, a]),
        Format::Rgbx8888 => Some([b, c, d, 255]),
        Format::Bgra8888 => Some([d, c, b, a]),
        Format::Bgrx8888 => Some([d, c, b, 255]),
        _ => None,
    }
}

fn transformed_size(width: usize, height: usize, transform: Transform) -> Option<(usize, usize)> {
    match transform {
        Transform::_90 | Transform::_270 | Transform::Flipped90 | Transform::Flipped270 => {
            Some((height, width))
        }
        Transform::Normal | Transform::_180 | Transform::Flipped | Transform::Flipped180 => {
            Some((width, height))
        }
        _ => None,
    }
}

fn transformed_pixel(
    x: usize,
    y: usize,
    width: usize,
    height: usize,
    transform: Transform,
) -> Option<(usize, usize)> {
    // Undo the client's surface-to-buffer transform. Coordinates address pixel
    // centers, hence -1 rather than the edge-coordinate geometry formulas.
    match transform {
        Transform::Normal => Some((x, y)),
        Transform::_90 => Some((y, width - 1 - x)),
        Transform::_180 => Some((width - 1 - x, height - 1 - y)),
        Transform::_270 => Some((height - 1 - y, x)),
        Transform::Flipped => Some((width - 1 - x, y)),
        Transform::Flipped90 => Some((height - 1 - y, width - 1 - x)),
        Transform::Flipped180 => Some((x, height - 1 - y)),
        Transform::Flipped270 => Some((y, x)),
        _ => None,
    }
}

fn read_bitmap(buffer: &WlBuffer, transform: Transform) -> Option<Retained<NSBitmapImageRep>> {
    with_buffer_contents(buffer, |ptr, len, data| {
        let (offset, stride) = buffer_layout(len, data.offset, data.width, data.height, data.stride)?;
        pixel_bgra(data.format, &[0; 4])?;
        let (width, height) = transformed_size(data.width as usize, data.height as usize, transform)?;
        let mtm = unsafe { MainThreadMarker::new_unchecked() };
        let bitmap = unsafe {
            NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bitmapFormat_bytesPerRow_bitsPerPixel(
                mtm.alloc(), std::ptr::null_mut(), width as isize, height as isize, 8, 4, true, false,
                NSDeviceRGBColorSpace, NSBitmapFormat::AlphaFirst | NSBitmapFormat::ThirtyTwoBitLittleEndian,
                (width * 4) as isize, 32,
            )
        }?;
        let destination = unsafe { bitmap.bitmapData() };
        if destination.is_null() { return None; }
        // AppKit owns this allocation; no SHM pointer escapes the pool lock and
        // no intermediate pixel Vec is needed for the usual ARGB cursor.
        let destination = unsafe { std::slice::from_raw_parts_mut(destination, width * height * 4) };
        let source = unsafe { std::slice::from_raw_parts(ptr, len) };
        for y in 0..data.height as usize {
            let row = &source[offset + y * stride..offset + y * stride + data.width as usize * 4];
            if transform == Transform::Normal && data.format == Format::Argb8888 {
                destination[y * width * 4..(y + 1) * width * 4].copy_from_slice(row);
            } else {
                for (x, pixel) in row.chunks_exact(4).enumerate() {
                    let (x, y) = transformed_pixel(x, y, data.width as usize, data.height as usize, transform)?;
                    let base = (y * width + x) * 4;
                    destination[base..base + 4].copy_from_slice(&pixel_bgra(data.format, pixel)?);
                }
            }
        }
        Some(bitmap)
    }).ok().flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_visibility_requests_are_balanced() {
        let visibility = CursorVisibility(AtomicBool::new(false));
        let transitions = [false, true, true, true, false, false, true, false, false]
            .into_iter()
            .filter_map(|hidden| visibility.update(hidden))
            .collect::<Vec<_>>();
        assert_eq!(transitions, [true, false, true, false]);
    }

    #[test]
    fn native_leave_preserves_explicit_hidden_request() {
        let mut cursor = NativeCursor::default();
        let focus = ObjectId::null();
        let owner = WindowId::from(1_u64);
        cursor.window_id = Some(owner);
        cursor.set_focus(Some(focus.clone()));
        cursor.set_image(CursorImageStatus::Hidden);
        cursor.left_window(owner);
        // This is the reset Smithay sends when the parent clears pointer focus.
        cursor.set_image(CursorImageStatus::default_named());
        assert_eq!(cursor.requested, CursorImageStatus::Hidden);
        cursor.set_focus(None);
        assert!(!cursor.focus.valid());
        cursor.set_focus(Some(focus));
        assert!(cursor.focus.valid());
        assert_eq!(cursor.requested, CursorImageStatus::Hidden);
        assert!(cursor.applied.is_none());
        // An out-of-window refresh must not issue any AppKit calls.
        cursor.refresh();
        assert!(cursor.applied.is_none());
    }

    #[test]
    fn different_surface_invalidates_previous_client_request() {
        let mut focus = CursorFocus::<u32>::default();
        assert!(focus.update(Some(1)));
        focus.request();
        assert!(focus.valid());
        assert!(!focus.update(None));
        assert!(!focus.valid());
        assert!(!focus.update(Some(1)));
        assert!(focus.valid());
        assert!(focus.update(Some(2)));
        assert!(focus.valid());
    }

    #[test]
    fn another_windows_leave_does_not_release_cursor_ownership() {
        let mut cursor = NativeCursor::default();
        let owner = WindowId::from(1_u64);
        cursor.window_id = Some(owner);
        cursor.set_image(CursorImageStatus::Hidden);
        cursor.left_window(WindowId::from(2_u64));
        assert_eq!(cursor.window_id, Some(owner));
        assert_eq!(cursor.requested, CursorImageStatus::Hidden);
        cursor.left_window(owner);
        assert!(cursor.window_id.is_none());
    }

    #[test]
    fn shape_request_does_not_clear_lock_state() {
        let mut cursor = NativeCursor::default();
        cursor.set_locked(true);
        cursor.set_image(CursorImageStatus::Named(CursorIcon::Pointer));
        assert!(cursor.locked);
        cursor.left();
        assert!(!cursor.locked);
        assert_eq!(
            cursor.requested,
            CursorImageStatus::Named(CursorIcon::Pointer)
        );
    }

    #[test]
    fn invalid_host_point_scale_does_not_change_cached_scale() {
        let mut cursor = NativeCursor::default();
        for scale in [f64::NAN, f64::INFINITY, 0.0, -1.0] {
            cursor.set_point_scale(scale);
            assert_eq!(cursor.points_per_logical, 1.0);
        }
        cursor.set_point_scale(0.5);
        assert_eq!(cursor.points_per_logical, 0.5);
    }

    #[test]
    fn hotspot_is_logical_and_not_divided_by_buffer_scale() {
        let geometry = Geometry {
            scale: 2,
            ..Geometry::default()
        };
        let (size, hotspot) = image_geometry(64, 48, geometry, (7, 9), 1.0).unwrap();
        assert_eq!(size, NSSize::new(32.0, 24.0));
        assert_eq!(hotspot, NSPoint::new(7.0, 9.0));
        let (size, hotspot) = image_geometry(64, 48, geometry, (7, 9), 0.5).unwrap();
        assert_eq!(size, NSSize::new(16.0, 12.0));
        assert_eq!(hotspot, NSPoint::new(3.5, 4.5));
    }

    #[test]
    fn viewport_destination_supports_fractional_cursor_scaling() {
        let geometry = Geometry {
            destination: Some((24, 24)),
            ..Geometry::default()
        };
        assert_eq!(
            image_geometry(30, 30, geometry, (3, 4), 1.0).unwrap().0,
            NSSize::new(24.0, 24.0)
        );
    }

    #[test]
    fn shm_layout_accepts_padding_and_offset_but_rejects_invalid_bounds() {
        assert_eq!(buffer_layout(148, 128, 2, 2, 12), Some((128, 12)));
        assert_eq!(buffer_layout(147, 128, 2, 2, 12), None);
        assert_eq!(buffer_layout(100, -1, 2, 2, 8), None);
        assert_eq!(buffer_layout(100, 0, 2, 2, 4), None);
        assert_eq!(buffer_layout(100, 0, 0, 2, 8), None);
        assert_eq!(buffer_layout(usize::MAX, 0, i32::MAX, 2, i32::MAX), None);
    }

    #[test]
    fn converts_all_advertised_shm_formats_with_premultiplied_alpha() {
        for (format, bytes, alpha) in [
            (Format::Argb8888, [1, 2, 3, 4], 4),
            (Format::Xrgb8888, [1, 2, 3, 0], 255),
            (Format::Abgr8888, [3, 2, 1, 4], 4),
            (Format::Xbgr8888, [3, 2, 1, 0], 255),
            (Format::Rgba8888, [4, 1, 2, 3], 4),
            (Format::Rgbx8888, [0, 1, 2, 3], 255),
            (Format::Bgra8888, [4, 3, 2, 1], 4),
            (Format::Bgrx8888, [0, 3, 2, 1], 255),
        ] {
            assert_eq!(pixel_bgra(format, &bytes), Some([1, 2, 3, alpha]));
        }
        assert_eq!(pixel_bgra(Format::Rgb565, &[0; 4]), None);
    }

    #[test]
    fn every_buffer_transform_is_a_bijection() {
        for transform in [
            Transform::Normal,
            Transform::_90,
            Transform::_180,
            Transform::_270,
            Transform::Flipped,
            Transform::Flipped90,
            Transform::Flipped180,
            Transform::Flipped270,
        ] {
            let (width, height) = transformed_size(3, 2, transform).unwrap();
            let mut seen = std::collections::HashSet::new();
            for y in 0..2 {
                for x in 0..3 {
                    let point = transformed_pixel(x, y, 3, 2, transform).unwrap();
                    assert!(point.0 < width && point.1 < height);
                    assert!(seen.insert(point));
                }
            }
            assert_eq!(seen.len(), 6);
        }
    }
}
