#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PresentationMode {
    #[default]
    Desktop,
    Rootless,
}

pub struct RootlessWindow {
    pub renderer: crate::metal_renderer::MetalRenderer,
    pub toplevel: smithay::wayland::shell::xdg::ToplevelSurface,
    pub scale_factor: f64,
    pub last_pointer: smithay::utils::Point<f64, smithay::utils::Logical>,
    pub presented_once: bool,
    pub last_geometry: Option<(i32, i32, i32, i32)>,
    pub last_render_metrics: Option<(u32, u32, u64, i32, i32, i32, i32)>,
}

impl RootlessWindow {
    pub fn surface_id(&self) -> smithay::reexports::wayland_server::backend::ObjectId {
        use smithay::reexports::wayland_server::Resource;
        self.toplevel.wl_surface().id()
    }
}

impl PresentationMode {
    pub const ENV: &'static str = "COCOA_WAY_PRESENTATION";

    pub fn parse(value: Option<&str>) -> Self {
        match value.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
            Some("rootless") => Self::Rootless,
            _ => Self::Desktop,
        }
    }

    pub fn from_env() -> Self {
        Self::parse(std::env::var(Self::ENV).ok().as_deref())
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Desktop => "desktop",
            Self::Rootless => "rootless",
        }
    }

    pub fn is_rootless(self) -> bool {
        self == Self::Rootless
    }
}

pub fn honor_rootless_maximize(presented_once: bool, maximized: bool) -> bool {
    !maximized || presented_once
}

pub fn toplevel_title(toplevel: &smithay::wayland::shell::xdg::ToplevelSurface) -> String {
    use smithay::wayland::shell::xdg::XdgToplevelSurfaceData;
    let metadata = smithay::wayland::compositor::with_states(toplevel.wl_surface(), |states| {
        states
            .data_map
            .get::<XdgToplevelSurfaceData>()
            .and_then(|data| {
                let data = data.lock().ok()?;
                Some((data.title.clone(), data.app_id.clone()))
            })
    });
    metadata
        .and_then(|(title, app_id)| title.or(app_id))
        .filter(|title| !title.trim().is_empty())
        .unwrap_or_else(|| "Cocoa-Way Application".into())
}

pub fn configure_toplevel(
    toplevel: &smithay::wayland::shell::xdg::ToplevelSurface,
    physical_width: u32,
    physical_height: u32,
    scale_factor: f64,
    maximized: bool,
    fullscreen: bool,
) {
    let (logical_width, logical_height) =
        crate::layout::logical_size_from_physical(physical_width, physical_height, scale_factor);
    toplevel.with_pending_state(|state| {
        use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State;

        state.size = Some((logical_width, logical_height).into());
        if maximized {
            state.states.set(State::Maximized);
        } else {
            state.states.unset(State::Maximized);
        }
        if fullscreen {
            state.states.set(State::Fullscreen);
        } else {
            state.states.unset(State::Fullscreen);
        }
    });
    smithay::wayland::compositor::with_states(toplevel.wl_surface(), |states| {
        smithay::wayland::fractional_scale::with_fractional_scale(states, |fractional_scale| {
            fractional_scale.set_preferred_scale(scale_factor);
        });
    });
    toplevel.send_configure();
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RootlessRenderOutcome {
    Submitted(usize),
    DeferredSubmission,
    RetryDrawable,
    Suspended,
}

pub fn render_rootless_window(
    rootless: &mut RootlessWindow,
    popups: &[smithay::wayland::shell::xdg::PopupSurface],
) -> RootlessRenderOutcome {
    use smithay::reexports::wayland_server::Resource;

    let current_scale = rootless.renderer.window.scale_factor();
    if (current_scale - rootless.scale_factor).abs() > f64::EPSILON {
        log::info!(
            "Rootless backing scale changed outside the event callback: {} -> {}",
            rootless.scale_factor,
            current_scale
        );
        rootless.scale_factor = current_scale;
        rootless.renderer.set_scale_factor(current_scale);
        let size = rootless.renderer.window.inner_size();
        configure_toplevel(
            &rootless.toplevel,
            size.width,
            size.height,
            current_scale,
            rootless.renderer.window.is_maximized(),
            rootless.renderer.window.fullscreen().is_some(),
        );
    }
    let size = rootless.renderer.window.inner_size();
    if size.width == 0 || size.height == 0 {
        return RootlessRenderOutcome::Suspended;
    }
    if size.width != rootless.renderer.width || size.height != rootless.renderer.height {
        rootless.renderer.resize(size.width, size.height);
    }
    if !rootless.renderer.clear(0.08, 0.08, 0.1, 1.0) {
        return RootlessRenderOutcome::RetryDrawable;
    }
    let scale = rootless.scale_factor;
    let root_surface = rootless.toplevel.wl_surface();
    let geometry = toplevel_window_geometry(&rootless.toplevel);
    let geometry_key = geometry.map(|geometry| {
        (
            geometry.loc.x,
            geometry.loc.y,
            geometry.size.w,
            geometry.size.h,
        )
    });
    if geometry_key != rootless.last_geometry {
        log::debug!(
            "Rootless geometry for {:?}: {:?}; native={}x{} scale={}",
            rootless.surface_id(),
            geometry_key,
            size.width,
            size.height,
            scale
        );
        rootless.last_geometry = geometry_key;
    }
    let mut rendered =
        render_toplevel_tree(&mut rootless.renderer, &rootless.toplevel, (0, 0), scale);
    rendered += render_toplevel_popups(&mut rootless.renderer, root_surface, popups, (0, 0), scale);

    let deferred = toplevel_has_pending_buffer_uploads(&rootless.toplevel, popups);
    if deferred {
        rootless.renderer.discard_trace_frame();
    }
    match rootless.renderer.swap_buffers() {
        Ok(true) => {}
        Ok(false) => return RootlessRenderOutcome::RetryDrawable,
        Err(error) => {
            log::error!("Failed to present rootless window: {error}");
            return RootlessRenderOutcome::RetryDrawable;
        }
    }
    if deferred {
        // A cached fallback can be presented without completing the new commit.
        return RootlessRenderOutcome::DeferredSubmission;
    }
    if let Some((buffer_width, buffer_height)) =
        rootless.renderer.cached_surface_size(&root_surface.id())
    {
        let mapping = smithay::wayland::compositor::with_states(root_surface, |states| {
            let mut attributes = states
                .cached_state
                .get::<smithay::wayland::compositor::SurfaceAttributes>();
            let current = attributes.current();
            crate::metal_renderer::geometry::SurfaceMapping {
                buffer_scale: current.buffer_scale,
                transform: current.buffer_transform,
                viewport: *states
                    .cached_state
                    .get::<smithay::wayland::viewporter::ViewportCachedState>()
                    .current(),
            }
        });
        let (destination_width, destination_height) = mapping
            .geometry((buffer_width, buffer_height), scale)
            .map_or((0, 0), |geometry| (geometry.width, geometry.height));
        let metrics = (
            size.width,
            size.height,
            scale.to_bits(),
            buffer_width,
            buffer_height,
            destination_width,
            destination_height,
        );
        if rootless.last_render_metrics != Some(metrics) {
            log::info!(
                "Rootless render {:?}: drawable={}x{} scale={} buffer={}x{}@{} destination={}x{}",
                rootless.surface_id(),
                size.width,
                size.height,
                scale,
                buffer_width,
                buffer_height,
                mapping.buffer_scale,
                destination_width,
                destination_height
            );
            rootless.last_render_metrics = Some(metrics);
        }
    }
    RootlessRenderOutcome::Submitted(rendered)
}

pub fn render_toplevel_tree(
    renderer: &mut crate::metal_renderer::MetalRenderer,
    toplevel: &smithay::wayland::shell::xdg::ToplevelSurface,
    window_origin: (i32, i32),
    scale: f64,
) -> usize {
    // xdg_surface.window_geometry excludes client-side shadows and other
    // out-of-window buffers. Map that geometry, rather than the raw root
    // surface, to the compositor window or tile origin.
    let root_origin = toplevel_window_geometry(toplevel)
        .map(|geometry| {
            (
                window_origin.0 - geometry.loc.x,
                window_origin.1 - geometry.loc.y,
            )
        })
        .unwrap_or(window_origin);
    render_surface_tree(renderer, toplevel.wl_surface(), root_origin, scale)
}

pub fn render_toplevel_popups(
    renderer: &mut crate::metal_renderer::MetalRenderer,
    root_surface: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
    popups: &[smithay::wayland::shell::xdg::PopupSurface],
    window_origin: (i32, i32),
    scale: f64,
) -> usize {
    let mut rendered = 0;
    for popup in popups {
        let Some((popup_root, popup_location)) = popup_root_and_location(popup, popups) else {
            continue;
        };
        if &popup_root != root_surface {
            continue;
        }
        rendered += render_surface_tree(
            renderer,
            popup.wl_surface(),
            (
                window_origin.0 + popup_location.x,
                window_origin.1 + popup_location.y,
            ),
            scale,
        );
    }
    rendered
}

/// Check after rendering and before clearing dirty state or sending callbacks.
/// Cached fallback draws do not complete a retained commit awaiting upload.
pub fn toplevel_has_pending_buffer_uploads(
    toplevel: &smithay::wayland::shell::xdg::ToplevelSurface,
    popups: &[smithay::wayland::shell::xdg::PopupSurface],
) -> bool {
    let root = toplevel.wl_surface();
    surface_tree_has_pending_buffer_uploads(root)
        || popups.iter().any(|popup| {
            popup_root_and_location(popup, popups)
                .is_some_and(|(popup_root, _)| &popup_root == root)
                && surface_tree_has_pending_buffer_uploads(popup.wl_surface())
        })
}

fn surface_tree_has_pending_buffer_uploads(
    root: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
) -> bool {
    use smithay::reexports::wayland_server::Resource;
    use smithay::wayland::compositor::{
        BufferAssignment, SurfaceAttributes, TraversalAction, with_surface_tree_downward,
    };

    let pending = std::cell::Cell::new(false);
    with_surface_tree_downward(
        root,
        (),
        |_, _, _| {
            if pending.get() {
                TraversalAction::Break
            } else {
                TraversalAction::DoChildren(())
            }
        },
        |surface, states, _| {
            if surface.is_alive()
                && matches!(
                    states
                        .cached_state
                        .get::<SurfaceAttributes>()
                        .current()
                        .buffer
                        .as_ref(),
                    Some(BufferAssignment::NewBuffer(_))
                )
            {
                pending.set(true);
            }
        },
        |_, _, _| !pending.get(),
    );
    pending.get()
}

pub fn surface_under(
    rootless: &RootlessWindow,
    popups: &[smithay::wayland::shell::xdg::PopupSurface],
    location: smithay::utils::Point<f64, smithay::utils::Logical>,
) -> Option<(
    smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
    smithay::utils::Point<f64, smithay::utils::Logical>,
)> {
    toplevel_surface_under_with_sizes(&rootless.toplevel, popups, (0, 0).into(), location, &|id| {
        rootless.renderer.cached_surface_size(id)
    })
}

/// Hit the desktop in reverse draw order, including popups/subsurfaces outside
/// their tile. Both `location` and the returned surface origin are desktop
/// logical coordinates; pass the origin unchanged to Smithay pointer motion.
pub fn desktop_surface_under(
    renderer: &crate::metal_renderer::MetalRenderer,
    tiles: &[crate::layout::Tile],
    popups: &[smithay::wayland::shell::xdg::PopupSurface],
    location: smithay::utils::Point<f64, smithay::utils::Logical>,
) -> Option<(
    smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
    smithay::utils::Point<f64, smithay::utils::Logical>,
)> {
    desktop_surface_under_with_sizes(tiles, popups, location, &|id| {
        renderer.cached_surface_size(id)
    })
}

fn desktop_surface_under_with_sizes(
    tiles: &[crate::layout::Tile],
    popups: &[smithay::wayland::shell::xdg::PopupSurface],
    location: smithay::utils::Point<f64, smithay::utils::Logical>,
    buffer_size: &impl Fn(&smithay::reexports::wayland_server::backend::ObjectId) -> Option<(i32, i32)>,
) -> Option<(
    smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
    smithay::utils::Point<f64, smithay::utils::Logical>,
)> {
    // Match rendering: each tile's tree, then its popups, then the next tile.
    // A tile rectangle is not a clip or an input region.
    tiles.iter().rev().find_map(|tile| {
        toplevel_surface_under_with_sizes(
            &tile.toplevel,
            popups,
            tile.position,
            location,
            buffer_size,
        )
    })
}

fn toplevel_surface_under_with_sizes(
    toplevel: &smithay::wayland::shell::xdg::ToplevelSurface,
    popups: &[smithay::wayland::shell::xdg::PopupSurface],
    window_origin: smithay::utils::Point<i32, smithay::utils::Logical>,
    location: smithay::utils::Point<f64, smithay::utils::Logical>,
    buffer_size: &impl Fn(&smithay::reexports::wayland_server::backend::ObjectId) -> Option<(i32, i32)>,
) -> Option<(
    smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
    smithay::utils::Point<f64, smithay::utils::Logical>,
)> {
    if !toplevel.alive() || !location.x.is_finite() || !location.y.is_finite() {
        return None;
    }
    let root_surface = toplevel.wl_surface();
    for popup in popups.iter().rev() {
        let Some((popup_root, popup_location)) = popup_root_and_location(popup, popups) else {
            continue;
        };
        if &popup_root == root_surface
            && let Some(origin) = checked_location_sum(window_origin, popup_location)
            && let Some(hit) = surface_tree_hit(buffer_size, popup.wl_surface(), origin, location)
        {
            return Some(hit);
        }
    }
    let root_origin = if let Some(geometry) = toplevel_window_geometry(toplevel) {
        (
            window_origin.x.checked_sub(geometry.loc.x)?,
            window_origin.y.checked_sub(geometry.loc.y)?,
        )
            .into()
    } else {
        window_origin
    };
    surface_tree_hit(buffer_size, root_surface, root_origin, location)
}

fn checked_location_sum(
    left: smithay::utils::Point<i32, smithay::utils::Logical>,
    right: smithay::utils::Point<i32, smithay::utils::Logical>,
) -> Option<smithay::utils::Point<i32, smithay::utils::Logical>> {
    Some((left.x.checked_add(right.x)?, left.y.checked_add(right.y)?).into())
}

fn toplevel_window_geometry(
    toplevel: &smithay::wayland::shell::xdg::ToplevelSurface,
) -> Option<smithay::utils::Rectangle<i32, smithay::utils::Logical>> {
    smithay::wayland::compositor::with_states(toplevel.wl_surface(), |states| {
        states
            .cached_state
            .get::<smithay::wayland::shell::xdg::SurfaceCachedState>()
            .current()
            .geometry
    })
}

fn popup_root_and_location(
    popup: &smithay::wayland::shell::xdg::PopupSurface,
    popups: &[smithay::wayland::shell::xdg::PopupSurface],
) -> Option<(
    smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
    smithay::utils::Point<i32, smithay::utils::Logical>,
)> {
    use smithay::reexports::wayland_server::Resource;

    if !popup.alive() {
        return None;
    }
    let mut parent = popup.get_parent_surface()?;
    let mut location = popup
        .with_committed_state(|state| state.map(|state| state.geometry.loc).unwrap_or_default());
    let mut visited = std::collections::HashSet::new();
    visited.insert(popup.wl_surface().id());
    loop {
        if !parent.is_alive() || !visited.insert(parent.id()) {
            return None;
        }
        if let Some(subsurface_parent) = smithay::wayland::compositor::get_parent(&parent) {
            let offset = smithay::wayland::compositor::with_states(&parent, |states| {
                states
                    .cached_state
                    .get::<smithay::wayland::compositor::SubsurfaceCachedState>()
                    .current()
                    .location
            });
            location = checked_location_sum(location, offset)?;
            parent = subsurface_parent;
            continue;
        }
        let Some(parent_popup) = popups
            .iter()
            .find(|candidate| candidate.wl_surface() == &parent)
        else {
            break;
        };
        let offset = parent_popup.with_committed_state(|state| {
            state.map(|state| state.geometry.loc).unwrap_or_default()
        });
        location = checked_location_sum(location, offset)?;
        parent = parent_popup.get_parent_surface()?;
    }
    Some((parent, location))
}

fn render_surface_tree(
    renderer: &mut crate::metal_renderer::MetalRenderer,
    root: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
    origin: (i32, i32),
    scale: f64,
) -> usize {
    use crate::metal_renderer::PixelDrawResult;
    use smithay::reexports::wayland_server::Resource;
    use smithay::wayland::compositor::{
        BufferAssignment, SubsurfaceCachedState, SurfaceAttributes, TraversalAction,
        with_surface_tree_upward,
    };

    let mut rendered = 0usize;
    // Render back-to-front. The downward traversal is intentionally reserved
    // for hit testing because it visits the visually topmost surface first.
    with_surface_tree_upward(
        root,
        origin,
        |_, states, location| {
            let offset = states
                .cached_state
                .get::<SubsurfaceCachedState>()
                .current()
                .location;
            TraversalAction::DoChildren((location.0 + offset.x, location.1 + offset.y))
        },
        |surface, states, location| {
            // The traversal value is the parent origin. Apply this surface's
            // subsurface offset here as well as when descending into children.
            // Simple clients usually attach to the root at (0, 0), which hid
            // this for a long time; Firefox composes much of its UI from
            // non-zero subsurfaces.
            let offset = states
                .cached_state
                .get::<SubsurfaceCachedState>()
                .current()
                .location;
            let mut attributes = states.cached_state.get::<SurfaceAttributes>();
            let current = attributes.current();
            let mapping = crate::metal_renderer::geometry::SurfaceMapping {
                buffer_scale: current.buffer_scale,
                transform: current.buffer_transform,
                viewport: *states
                    .cached_state
                    .get::<smithay::wayland::viewporter::ViewportCachedState>()
                    .current(),
            };
            let x = (f64::from(location.0 + offset.x) * scale).round() as i32;
            let y = (f64::from(location.1 + offset.y) * scale).round() as i32;
            let surface_id = surface.id();
            let buffer_assignment = current.buffer.take();
            let damage = std::mem::take(&mut current.damage);
            match buffer_assignment {
                Some(BufferAssignment::NewBuffer(buffer)) => {
                    let buffer_scale = current.buffer_scale.max(1);
                    let buffer_id = buffer.id();
                    let result = crate::render::with_buffer_pixels(
                        &buffer,
                        |width, height, bytes_per_row, pixels, alpha| {
                            let buffer_damage = buffer_damage_rects(
                                &damage,
                                width,
                                height,
                                buffer_scale,
                                current.buffer_transform,
                                mapping.viewport.src.is_some() || mapping.viewport.dst.is_some(),
                            );
                            let Some(geometry) = mapping.geometry((width, height), scale) else {
                                log::warn!(
                                    "Invalid committed surface/viewport geometry for {surface_id:?}"
                                );
                                return PixelDrawResult::Rejected;
                            };
                            renderer.draw_pixels(
                                surface_id.clone(),
                                buffer_id,
                                x,
                                y,
                                geometry,
                                width,
                                height,
                                bytes_per_row,
                                pixels,
                                alpha,
                                &buffer_damage,
                            )
                        },
                    )
                    // Snapshot preparation failures are terminal for this commit.
                    .unwrap_or(PixelDrawResult::Rejected);
                    rendered += finish_buffer_upload(
                        current,
                        buffer,
                        damage,
                        result,
                        || renderer.draw_from_cache(&surface_id, x, y, scale, mapping),
                        |buffer| buffer.release(),
                    );
                }
                Some(BufferAssignment::Removed) => renderer.evict_texture(&surface_id),
                None => {
                    if renderer.draw_from_cache(&surface_id, x, y, scale, mapping) {
                        rendered += 1;
                    }
                }
            }
        },
        |_, _, _| true,
    );
    rendered
}

fn finish_buffer_upload(
    current: &mut smithay::wayland::compositor::SurfaceAttributes,
    buffer: smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer,
    damage: Vec<smithay::wayland::compositor::Damage>,
    result: crate::metal_renderer::PixelDrawResult,
    draw_cached: impl FnOnce() -> bool,
    release: impl FnOnce(&smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer),
) -> usize {
    use crate::metal_renderer::PixelDrawResult;
    use smithay::wayland::compositor::BufferAssignment;

    match result {
        PixelDrawResult::Deferred => {
            // The surface lock prevents a newer commit while we restore this
            // assignment and its original (not buffer-converted) damage.
            current.buffer = Some(BufferAssignment::NewBuffer(buffer));
            current.damage = damage;
            let _ = draw_cached();
            0
        }
        PixelDrawResult::Drawn => {
            release(&buffer);
            1
        }
        PixelDrawResult::Rejected => {
            let rendered = usize::from(draw_cached());
            release(&buffer);
            rendered
        }
    }
}

fn buffer_damage_rects(
    damage: &[smithay::wayland::compositor::Damage],
    width: i32,
    height: i32,
    buffer_scale: i32,
    transform: smithay::reexports::wayland_server::protocol::wl_output::Transform,
    has_viewport: bool,
) -> Vec<crate::metal_renderer::BufferDamage> {
    use smithay::reexports::wayland_server::protocol::wl_output::Transform;
    use smithay::wayland::compositor::Damage;

    if damage.is_empty() {
        return Vec::new();
    }
    damage
        .iter()
        .map(|damage| match damage {
            Damage::Buffer(rect) => crate::metal_renderer::BufferDamage {
                x: rect.loc.x,
                y: rect.loc.y,
                width: rect.size.w,
                height: rect.size.h,
            },
            Damage::Surface(_) if transform != Transform::Normal || has_viewport => {
                crate::metal_renderer::BufferDamage {
                    x: 0,
                    y: 0,
                    width,
                    height,
                }
            }
            Damage::Surface(rect) => crate::metal_renderer::BufferDamage {
                x: rect.loc.x.saturating_mul(buffer_scale),
                y: rect.loc.y.saturating_mul(buffer_scale),
                width: rect.size.w.saturating_mul(buffer_scale),
                height: rect.size.h.saturating_mul(buffer_scale),
            },
        })
        .collect()
}

fn surface_tree_hit(
    buffer_size: &impl Fn(&smithay::reexports::wayland_server::backend::ObjectId) -> Option<(i32, i32)>,
    root: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
    origin: smithay::utils::Point<i32, smithay::utils::Logical>,
    point: smithay::utils::Point<f64, smithay::utils::Logical>,
) -> Option<(
    smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
    smithay::utils::Point<f64, smithay::utils::Logical>,
)> {
    use smithay::reexports::wayland_server::Resource;
    use smithay::wayland::compositor::{
        SubsurfaceCachedState, SurfaceAttributes, TraversalAction, with_surface_tree_downward,
    };

    let hit = std::cell::RefCell::new(None);
    with_surface_tree_downward(
        root,
        origin,
        |_, states, location| {
            if hit.borrow().is_some() {
                return TraversalAction::Break;
            }
            let offset = states
                .cached_state
                .get::<SubsurfaceCachedState>()
                .current()
                .location;
            match checked_location_sum(*location, offset) {
                Some(location) => TraversalAction::DoChildren(location),
                None => TraversalAction::SkipChildren,
            }
        },
        |surface, states, location| {
            // Smithay's post-filter runs after all siblings, including any
            // below the parent. Never let a later processor overwrite a hit.
            if hit.borrow().is_some() || !surface.is_alive() {
                return;
            }
            let offset = states
                .cached_state
                .get::<SubsurfaceCachedState>()
                .current()
                .location;
            let Some(surface_location) = checked_location_sum(*location, offset) else {
                return;
            };
            let surface_id = surface.id();
            let Some(size) = buffer_size(&surface_id) else {
                return;
            };
            let mut attributes = states.cached_state.get::<SurfaceAttributes>();
            let current = attributes.current();
            let viewport = *states
                .cached_state
                .get::<smithay::wayland::viewporter::ViewportCachedState>()
                .current();
            let size = surface_logical_size(
                size,
                current.buffer_scale,
                current.buffer_transform,
                viewport,
            );
            let local = point - surface_location.to_f64();
            let within_buffer =
                local.x >= 0.0 && local.y >= 0.0 && local.x < size.w && local.y < size.h;
            let within_input = current.input_region.as_ref().is_none_or(|region| {
                region.contains((local.x.floor() as i32, local.y.floor() as i32))
            });
            if within_buffer && within_input {
                *hit.borrow_mut() = Some((surface.clone(), surface_location.to_f64()));
            }
        },
        |_, _, _| hit.borrow().is_none(),
    );
    hit.into_inner()
}

/// Logical surface bounds after buffer transform/scale and optional viewport.
/// A source crop changes the size even when no destination was specified.
pub(crate) fn surface_logical_size(
    buffer_size: (i32, i32),
    buffer_scale: i32,
    transform: smithay::reexports::wayland_server::protocol::wl_output::Transform,
    viewport: smithay::wayland::viewporter::ViewportCachedState,
) -> smithay::utils::Size<f64, smithay::utils::Logical> {
    crate::metal_renderer::geometry::SurfaceMapping {
        buffer_scale,
        transform,
        viewport,
    }
    .logical_size(buffer_size)
    .unwrap_or_else(|| (0.0, 0.0).into())
}

#[cfg(test)]
mod tests {
    use super::{PresentationMode, honor_rootless_maximize};
    use crate::metal_renderer::PixelDrawResult;

    use smithay::{
        delegate_compositor, delegate_viewporter, delegate_xdg_shell,
        reexports::wayland_server::{
            Client, Display, DisplayHandle, Resource,
            backend::{ClientData, ObjectId},
            protocol::{
                wl_buffer::WlBuffer, wl_output::Transform, wl_seat::WlSeat, wl_surface::WlSurface,
            },
        },
        utils::{Logical, Point, Rectangle, Serial},
        wayland::{
            compositor::{
                self, BufferAssignment, CompositorClientState, CompositorHandler, CompositorState,
                Damage, RectangleKind, RegionAttributes, SubsurfaceCachedState, SurfaceAttributes,
            },
            shell::xdg::{
                PopupCachedState, PopupConfigure, PopupState, PopupSurface, PositionerState,
                SurfaceCachedState, ToplevelSurface, XdgShellHandler, XdgShellState,
            },
            viewporter::{ViewportCachedState, ViewporterState},
        },
    };
    use std::{
        collections::HashMap,
        io::{Read, Write},
        os::unix::net::UnixStream,
        sync::Arc,
    };

    #[derive(Default)]
    struct TestClient(CompositorClientState);
    impl ClientData for TestClient {}

    struct TestState {
        compositor: CompositorState,
        shell: XdgShellState,
        _viewporter: ViewporterState,
        toplevels: Vec<ToplevelSurface>,
        popups: Vec<PopupSurface>,
    }

    impl CompositorHandler for TestState {
        fn compositor_state(&mut self) -> &mut CompositorState {
            &mut self.compositor
        }
        fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
            &client.get_data::<TestClient>().unwrap().0
        }
        fn commit(&mut self, _surface: &WlSurface) {}
    }

    impl XdgShellHandler for TestState {
        fn xdg_shell_state(&mut self) -> &mut XdgShellState {
            &mut self.shell
        }
        fn new_toplevel(&mut self, surface: ToplevelSurface) {
            self.toplevels.push(surface);
        }
        fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
            self.popups.push(surface);
        }
        fn grab(&mut self, _surface: PopupSurface, _seat: WlSeat, _serial: Serial) {}
        fn reposition_request(
            &mut self,
            _surface: PopupSurface,
            _positioner: PositionerState,
            _token: u32,
        ) {
        }
    }

    delegate_compositor!(TestState);
    delegate_xdg_shell!(TestState);
    delegate_viewporter!(TestState);

    // Real Smithay trees over a socket pair, with modeled committed geometry
    // and texture sizes. No Metal device, native window, or pixel upload needed.
    struct Scene {
        display: Display<TestState>,
        state: TestState,
        socket: UnixStream,
        client: Client,
        next_id: u32,
        compositor: u32,
        subcompositor: u32,
        shell: u32,
        viewporter: u32,
        sizes: HashMap<ObjectId, (i32, i32)>,
        subsurfaces: HashMap<ObjectId, u32>,
    }

    impl Scene {
        fn new() -> Self {
            let display = Display::new().unwrap();
            let mut dh = display.handle();
            let state = TestState {
                compositor: CompositorState::new::<TestState>(&dh),
                shell: XdgShellState::new::<TestState>(&dh),
                _viewporter: ViewporterState::new::<TestState>(&dh),
                toplevels: Vec::new(),
                popups: Vec::new(),
            };
            let (socket, server) = UnixStream::pair().unwrap();
            socket
                .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                .unwrap();
            socket
                .set_write_timeout(Some(std::time::Duration::from_secs(2)))
                .unwrap();
            let client = dh
                .insert_client(server, Arc::new(TestClient::default()))
                .unwrap();
            let mut scene = Self {
                display,
                state,
                socket,
                client,
                next_id: 2,
                compositor: 0,
                subcompositor: 0,
                shell: 0,
                viewporter: 0,
                sizes: HashMap::new(),
                subsurfaces: HashMap::new(),
            };
            let registry = scene.id();
            scene.request(1, 1, &[registry]);
            let globals: HashMap<_, _> = scene
                .sync()
                .into_iter()
                .filter_map(|(id, opcode, body)| {
                    if id != registry || opcode != 0 {
                        return None;
                    }
                    let length = Self::word(&body[4..]) as usize;
                    let interface = String::from_utf8(body[8..8 + length - 1].to_vec()).unwrap();
                    Some((interface, Self::word(&body)))
                })
                .collect();
            scene.compositor = scene.bind(registry, globals["wl_compositor"], "wl_compositor");
            scene.subcompositor =
                scene.bind(registry, globals["wl_subcompositor"], "wl_subcompositor");
            scene.shell = scene.bind(registry, globals["xdg_wm_base"], "xdg_wm_base");
            scene.viewporter = scene.bind(registry, globals["wp_viewporter"], "wp_viewporter");
            scene.sync();
            scene
        }

        fn word(bytes: &[u8]) -> u32 {
            u32::from_ne_bytes(bytes[..4].try_into().unwrap())
        }
        fn id(&mut self) -> u32 {
            let id = self.next_id;
            self.next_id += 1;
            id
        }

        fn write_request(&mut self, object: u32, opcode: u16, body: &[u8]) {
            let mut bytes = object.to_ne_bytes().to_vec();
            bytes.extend((((body.len() as u32 + 8) << 16) | opcode as u32).to_ne_bytes());
            bytes.extend(body);
            self.socket.write_all(&bytes).unwrap();
        }

        fn request(&mut self, object: u32, opcode: u16, args: &[u32]) {
            let body: Vec<_> = args.iter().flat_map(|arg| arg.to_ne_bytes()).collect();
            self.write_request(object, opcode, &body);
        }

        fn bind(&mut self, registry: u32, global: u32, interface: &str) -> u32 {
            let id = self.id();
            let mut body = global.to_ne_bytes().to_vec();
            body.extend(((interface.len() + 1) as u32).to_ne_bytes());
            body.extend(interface.as_bytes());
            body.push(0);
            body.resize(body.len().next_multiple_of(4), 0);
            body.extend(1u32.to_ne_bytes());
            body.extend(id.to_ne_bytes());
            self.write_request(registry, 0, &body);
            id
        }

        fn sync(&mut self) -> Vec<(u32, u16, Vec<u8>)> {
            let callback = self.id();
            self.request(1, 0, &[callback]);
            self.display.dispatch_clients(&mut self.state).unwrap();
            self.display.flush_clients().unwrap();
            let mut events = Vec::new();
            loop {
                let mut header = [0; 8];
                self.socket.read_exact(&mut header).unwrap();
                let id = Self::word(&header);
                let opcode = Self::word(&header[4..]) as u16;
                let size = (Self::word(&header[4..]) >> 16) as usize;
                assert!(size >= 8);
                let mut body = vec![0; size - 8];
                self.socket.read_exact(&mut body).unwrap();
                assert!(
                    !(id == 1 && opcode == 0),
                    "unexpected protocol error: {body:?}"
                );
                if id == callback {
                    return events;
                }
                events.push((id, opcode, body));
            }
        }

        fn surface(&mut self, size: (i32, i32)) -> WlSurface {
            let id = self.id();
            self.request(self.compositor, 0, &[id]);
            self.sync();
            let surface = self
                .client
                .object_from_protocol_id::<WlSurface>(&self.display.handle(), id)
                .unwrap();
            self.sizes.insert(surface.id(), size);
            surface
        }

        fn toplevel(&mut self, size: (i32, i32)) -> (ToplevelSurface, u32) {
            let surface = self.surface(size);
            let xdg = self.id();
            let toplevel = self.id();
            self.request(self.shell, 2, &[xdg, surface.id().protocol_id()]);
            self.request(xdg, 1, &[toplevel]);
            self.sync();
            (self.state.toplevels.last().unwrap().clone(), xdg)
        }

        fn popup(
            &mut self,
            parent: u32,
            origin: (i32, i32),
            size: (i32, i32),
        ) -> (PopupSurface, u32) {
            let surface = self.surface(size);
            let xdg = self.id();
            let positioner = self.id();
            let popup = self.id();
            self.request(self.shell, 2, &[xdg, surface.id().protocol_id()]);
            self.request(self.shell, 1, &[positioner]);
            self.request(positioner, 1, &[size.0 as u32, size.1 as u32]);
            self.request(positioner, 2, &[0, 0, 1, 1]);
            self.request(xdg, 2, &[popup, parent, positioner]);
            self.sync();
            let popup = self.state.popups.last().unwrap().clone();
            compositor::with_states(&surface, |states| {
                states
                    .cached_state
                    .get::<PopupCachedState>()
                    .current()
                    .last_acked = Some(PopupConfigure {
                    serial: 1.into(),
                    state: PopupState {
                        geometry: Rectangle::new(origin.into(), size.into()),
                        positioner: PositionerState::default(),
                    },
                    reposition_token: None,
                });
            });
            (popup, xdg)
        }

        fn subsurface(
            &mut self,
            parent: &WlSurface,
            origin: (i32, i32),
            size: (i32, i32),
        ) -> WlSurface {
            let surface = self.surface(size);
            let sub = self.id();
            self.request(
                self.subcompositor,
                1,
                &[sub, surface.id().protocol_id(), parent.id().protocol_id()],
            );
            self.sync();
            compositor::with_states(&surface, |states| {
                states
                    .cached_state
                    .get::<SubsurfaceCachedState>()
                    .current()
                    .location = origin.into();
            });
            self.subsurfaces.insert(surface.id(), sub);
            surface
        }

        fn hit(
            &self,
            tiles: &[crate::layout::Tile],
            point: (f64, f64),
        ) -> Option<(WlSurface, Point<f64, Logical>)> {
            super::desktop_surface_under_with_sizes(
                tiles,
                &self.state.popups,
                point.into(),
                &|id| self.sizes.get(id).copied(),
            )
        }
    }

    fn input_region(surface: &WlSurface, region: Option<RegionAttributes>) {
        compositor::with_states(surface, |states| {
            states
                .cached_state
                .get::<SurfaceAttributes>()
                .current()
                .input_region = region;
        });
    }

    fn tile(toplevel: ToplevelSurface, origin: (i32, i32)) -> crate::layout::Tile {
        crate::layout::Tile::new(toplevel, origin.into(), (100, 100).into())
    }

    // Ownership/release bookkeeping needs only an inert protocol handle. The
    // injected release callback records calls without a GPU or SHM allocation.
    fn inert_buffer(dh: &DisplayHandle) -> WlBuffer {
        WlBuffer::from_id(dh, ObjectId::null()).unwrap()
    }

    fn original_upload_damage() -> Vec<Damage> {
        vec![
            Damage::Surface(Rectangle::new((-2, 3).into(), (7, 9).into())),
            Damage::Buffer(Rectangle::new((8, 11).into(), (13, 17).into())),
        ]
    }

    #[test]
    fn deferred_upload_retains_assignment_and_original_damage_without_release() {
        let display = Display::<()>::new().unwrap();
        let buffer = inert_buffer(&display.handle());
        for cached in [false, true] {
            let mut current = SurfaceAttributes::default();
            let damage = original_upload_damage();
            let damage_allocation = damage.as_ptr();
            let releases = std::cell::Cell::new(0);
            let cache_draws = std::cell::Cell::new(0);
            let rendered = super::finish_buffer_upload(
                &mut current,
                buffer.clone(),
                damage,
                PixelDrawResult::Deferred,
                || {
                    cache_draws.set(cache_draws.get() + 1);
                    cached
                },
                |_| releases.set(releases.get() + 1),
            );
            assert_eq!(rendered, 0);
            assert_eq!(releases.get(), 0);
            assert_eq!(cache_draws.get(), 1);
            assert!(
                matches!(current.buffer.as_ref(), Some(BufferAssignment::NewBuffer(retained)) if retained == &buffer)
            );
            assert_eq!(current.damage, original_upload_damage());
            assert_eq!(current.damage.as_ptr(), damage_allocation);
        }
    }

    #[test]
    fn deferred_upload_retries_then_releases_once_after_copy() {
        let display = Display::<()>::new().unwrap();
        let buffer = inert_buffer(&display.handle());
        let mut current = SurfaceAttributes::default();
        current.buffer = Some(BufferAssignment::NewBuffer(buffer));
        current.damage = original_upload_damage();
        let releases = std::cell::Cell::new(0);
        for result in [
            PixelDrawResult::Deferred,
            PixelDrawResult::Deferred,
            PixelDrawResult::Drawn,
        ] {
            let Some(BufferAssignment::NewBuffer(buffer)) = current.buffer.take() else {
                panic!("deferred upload lost its buffer assignment");
            };
            let damage = std::mem::take(&mut current.damage);
            assert_eq!(damage, original_upload_damage());
            let rendered = super::finish_buffer_upload(
                &mut current,
                buffer,
                damage,
                result,
                || {
                    assert_ne!(result, PixelDrawResult::Drawn);
                    true
                },
                |_| releases.set(releases.get() + 1),
            );
            assert_eq!(rendered, usize::from(result == PixelDrawResult::Drawn));
            assert_eq!(
                releases.get(),
                usize::from(result == PixelDrawResult::Drawn)
            );
        }
        assert!(current.buffer.is_none());
        assert!(current.damage.is_empty());
    }

    #[test]
    fn rejected_upload_releases_once_and_can_draw_the_previous_cache() {
        let display = Display::<()>::new().unwrap();
        for cached in [false, true] {
            let mut current = SurfaceAttributes::default();
            let releases = std::cell::Cell::new(0);
            let rendered = super::finish_buffer_upload(
                &mut current,
                inert_buffer(&display.handle()),
                original_upload_damage(),
                PixelDrawResult::Rejected,
                || cached,
                |_| releases.set(releases.get() + 1),
            );
            assert_eq!(rendered, usize::from(cached));
            assert_eq!(releases.get(), 1);
            assert!(current.buffer.is_none());
            assert!(current.damage.is_empty());
        }
    }

    #[test]
    fn pending_upload_query_covers_subsurfaces_and_popups_of_only_this_toplevel() {
        let mut scene = Scene::new();
        let (root, xdg) = scene.toplevel((100, 100));
        let (other, other_xdg) = scene.toplevel((100, 100));
        let (popup, _) = scene.popup(xdg, (5, 5), (20, 20));
        let (other_popup, _) = scene.popup(other_xdg, (5, 5), (20, 20));
        let child = scene.subsurface(root.wl_surface(), (1, 1), (10, 10));
        let popup_child = scene.subsurface(popup.wl_surface(), (1, 1), (10, 10));
        let buffer = inert_buffer(&scene.display.handle());
        let pending = |surface: &WlSurface, assignment| {
            compositor::with_states(surface, |states| {
                states
                    .cached_state
                    .get::<SurfaceAttributes>()
                    .current()
                    .buffer = assignment;
            });
        };
        pending(
            other_popup.wl_surface(),
            Some(BufferAssignment::NewBuffer(buffer.clone())),
        );
        assert!(!super::toplevel_has_pending_buffer_uploads(
            &root,
            &scene.state.popups
        ));
        assert!(super::toplevel_has_pending_buffer_uploads(
            &other,
            &scene.state.popups
        ));
        for surface in [root.wl_surface(), &child, popup.wl_surface(), &popup_child] {
            pending(surface, Some(BufferAssignment::NewBuffer(buffer.clone())));
            assert!(super::toplevel_has_pending_buffer_uploads(
                &root,
                &scene.state.popups
            ));
            pending(surface, Some(BufferAssignment::Removed));
            assert!(!super::toplevel_has_pending_buffer_uploads(
                &root,
                &scene.state.popups
            ));
            pending(surface, None);
        }
    }

    #[test]
    fn desktop_hits_popups_outside_the_tile_and_nested_popup_subsurfaces() {
        let mut scene = Scene::new();
        let (root, xdg) = scene.toplevel((100, 100));
        let (popup, popup_xdg) = scene.popup(xdg, (120, 15), (40, 30));
        let (nested, _) = scene.popup(popup_xdg, (10, -10), (20, 20));
        let child = scene.subsurface(nested.wl_surface(), (-5, 2), (10, 10));
        let tiles = [tile(root, (100, 50))];
        assert_eq!(
            scene.hit(&tiles, (225.0, 80.0)),
            Some((popup.wl_surface().clone(), (220.0, 65.0).into()))
        );
        assert_eq!(
            scene.hit(&tiles, (240.0, 60.0)),
            Some((nested.wl_surface().clone(), (230.0, 55.0).into()))
        );
        assert_eq!(
            scene.hit(&tiles, (226.5, 58.5)),
            Some((child, (225.0, 57.0).into()))
        );
    }

    #[test]
    fn desktop_subsurface_offsets_and_input_regions_are_surface_local() {
        let mut scene = Scene::new();
        let (root, _) = scene.toplevel((100, 100));
        compositor::with_states(root.wl_surface(), |states| {
            states
                .cached_state
                .get::<SurfaceCachedState>()
                .current()
                .geometry = Some(Rectangle::new((10, 20).into(), (90, 80).into()));
        });
        let child = scene.subsurface(root.wl_surface(), (30, 40), (60, 30));
        let grandchild = scene.subsurface(&child, (5, -10), (20, 10));
        let tiles = [tile(root.clone(), (100, 200))];
        input_region(
            &child,
            Some(RegionAttributes {
                rects: vec![
                    (
                        RectangleKind::Add,
                        Rectangle::new((8, 4).into(), (20, 12).into()),
                    ),
                    (
                        RectangleKind::Subtract,
                        Rectangle::new((10, 6).into(), (4, 4).into()),
                    ),
                ],
            }),
        );
        assert_eq!(
            scene.hit(&tiles, (128.5, 224.5)),
            Some((child.clone(), (120.0, 220.0).into()))
        );
        assert_eq!(
            scene.hit(&tiles, (131.0, 227.0)),
            Some((root.wl_surface().clone(), (90.0, 180.0).into()))
        );
        input_region(&child, Some(RegionAttributes::default()));
        assert_eq!(
            scene.hit(&tiles, (127.5, 213.5)),
            Some((grandchild, (125.0, 210.0).into()))
        );
        input_region(root.wl_surface(), Some(RegionAttributes::default()));
        assert!(scene.hit(&tiles, (105.0, 205.0)).is_none());
    }

    #[test]
    fn desktop_hit_order_matches_tile_draw_order_and_input_passthrough() {
        let mut scene = Scene::new();
        let (lower, lower_xdg) = scene.toplevel((100, 100));
        let (popup, _) = scene.popup(lower_xdg, (120, 15), (40, 30));
        let (upper, _) = scene.toplevel((100, 100));
        let tiles = [tile(lower, (0, 0)), tile(upper.clone(), (100, 0))];
        assert_eq!(
            scene.hit(&tiles, (125.0, 20.0)),
            Some((upper.wl_surface().clone(), (100.0, 0.0).into()))
        );
        input_region(upper.wl_surface(), Some(RegionAttributes::default()));
        assert_eq!(
            scene.hit(&tiles, (125.0, 20.0)),
            Some((popup.wl_surface().clone(), (120.0, 15.0).into()))
        );
        scene.sizes.remove(&popup.wl_surface().id());
        assert!(scene.hit(&tiles, (125.0, 20.0)).is_none());
        assert!(scene.hit(&tiles, (f64::NAN, 20.0)).is_none());
    }

    #[test]
    fn desktop_parent_hit_is_not_overwritten_by_a_subsurface_below_it() {
        let mut scene = Scene::new();
        let (root, _) = scene.toplevel((100, 100));
        let child = scene.subsurface(root.wl_surface(), (5, 5), (20, 20));
        let sub = scene.subsurfaces[&child.id()];
        scene.request(sub, 3, &[root.wl_surface().id().protocol_id()]); // place_below
        scene.sync();
        let tiles = [tile(root.clone(), (100, 50))];
        assert_eq!(
            scene.hit(&tiles, (110.0, 60.0)),
            Some((root.wl_surface().clone(), (100.0, 50.0).into()))
        );
        input_region(root.wl_surface(), Some(RegionAttributes::default()));
        assert_eq!(
            scene.hit(&tiles, (110.0, 60.0)),
            Some((child, (105.0, 55.0).into()))
        );
    }

    #[test]
    fn desktop_hit_bounds_use_committed_viewport_source_and_destination() {
        let mut scene = Scene::new();
        let (root, _) = scene.toplevel((200, 100));
        let tiles = [tile(root.clone(), (0, 0))];
        compositor::with_states(root.wl_surface(), |states| {
            states
                .cached_state
                .get::<SurfaceAttributes>()
                .current()
                .buffer_scale = 2;
            states
                .cached_state
                .get::<ViewportCachedState>()
                .current()
                .src = Some(Rectangle::new((10.0, 5.0).into(), (30.0, 20.0).into()));
        });
        assert!(scene.hit(&tiles, (29.5, 19.5)).is_some());
        assert!(scene.hit(&tiles, (30.0, 10.0)).is_none());
        compositor::with_states(root.wl_surface(), |states| {
            states
                .cached_state
                .get::<ViewportCachedState>()
                .current()
                .dst = Some((80, 40).into());
        });
        assert!(scene.hit(&tiles, (79.5, 39.5)).is_some());
        assert!(scene.hit(&tiles, (80.0, 10.0)).is_none());
    }

    #[test]
    fn logical_surface_bounds_apply_transform_before_buffer_scale() {
        assert_eq!(
            super::surface_logical_size(
                (200, 100),
                2,
                Transform::_90,
                ViewportCachedState::default()
            ),
            (50.0, 100.0).into()
        );
        assert_eq!(
            super::surface_logical_size(
                (200, 100),
                2,
                Transform::Flipped270,
                ViewportCachedState::default()
            ),
            (50.0, 100.0).into()
        );
        assert_eq!(
            super::surface_logical_size(
                (200, 100),
                0,
                Transform::Normal,
                ViewportCachedState::default()
            ),
            (200.0, 100.0).into()
        );
    }

    #[test]
    fn viewport_protocol_commit_changes_both_render_geometry_and_desktop_rootless_hits() {
        let mut scene = Scene::new();
        let (root, _) = scene.toplevel((200, 100));
        let surface = root.wl_surface();
        let surface_id = surface.id().protocol_id();
        let viewport = scene.id();
        scene.request(scene.viewporter, 1, &[viewport, surface_id]);
        scene.request(viewport, 1, &[2688, 1344, 30 * 256, 20 * 256]); // (10.5, 5.25)
        scene.sync();
        compositor::with_states(surface, |states| {
            let mut attributes = states.cached_state.get::<SurfaceAttributes>();
            attributes.pending().buffer_scale = 2;
            attributes.pending().buffer_transform = Transform::_90;
        });
        let tiles = [tile(root.clone(), (100, 50))];
        // Requests are pending: neither renderer geometry nor input may see them yet.
        assert!(scene.hit(&tiles, (160.0, 80.0)).is_some());
        let mapping = || {
            compositor::with_states(surface, |states| {
                let mut attributes = states.cached_state.get::<SurfaceAttributes>();
                let attributes = attributes.current();
                crate::metal_renderer::geometry::SurfaceMapping {
                    buffer_scale: attributes.buffer_scale,
                    transform: attributes.buffer_transform,
                    viewport: *states.cached_state.get::<ViewportCachedState>().current(),
                }
            })
        };
        assert_eq!(mapping().geometry((200, 100), 1.5).unwrap().width, 300);
        scene.request(surface_id, 6, &[]); // wl_surface.commit, no new buffer
        scene.sync();
        let geometry = mapping().geometry((200, 100), 1.5).unwrap();
        assert_eq!((geometry.width, geometry.height), (45, 30));
        assert_eq!(geometry.uv[2], [0.9475, 0.21]);
        assert!(scene.hit(&tiles, (129.99, 69.99)).is_some());
        assert!(scene.hit(&tiles, (130.0, 60.0)).is_none());
        let rootless_hit = |scene: &Scene, point| {
            super::toplevel_surface_under_with_sizes(
                &root,
                &scene.state.popups,
                (0, 0).into(),
                point,
                &|id| scene.sizes.get(id).copied(),
            )
        };
        assert_eq!(
            rootless_hit(&scene, (29.99, 19.99).into()),
            Some((surface.clone(), (0.0, 0.0).into()))
        );
        assert!(rootless_hit(&scene, (30.0, 10.0).into()).is_none());

        scene.request(viewport, 2, &[80, 40]);
        scene.sync();
        assert!(rootless_hit(&scene, (79.0, 39.0).into()).is_none());
        scene.request(surface_id, 6, &[]);
        scene.sync();
        let resized = mapping().geometry((200, 100), 1.5).unwrap();
        assert_eq!((resized.width, resized.height), (120, 60));
        assert_eq!(resized.uv, geometry.uv);
        assert!(scene.hit(&tiles, (179.99, 89.99)).is_some());
        assert!(rootless_hit(&scene, (79.99, 39.99).into()).is_some());
        assert!(rootless_hit(&scene, (80.0, 10.0).into()).is_none());

        scene.request(viewport, 0, &[]); // destroy is itself double-buffered
        scene.sync();
        assert_eq!(mapping().geometry((200, 100), 1.0).unwrap().width, 80);
        scene.request(surface_id, 6, &[]);
        scene.sync();
        let uncropped = mapping().geometry((200, 100), 1.0).unwrap();
        assert_eq!((uncropped.width, uncropped.height), (50, 100));
        assert!(rootless_hit(&scene, (49.99, 99.99).into()).is_some());
        assert!(rootless_hit(&scene, (50.0, 10.0).into()).is_none());
    }

    #[test]
    fn crop_surface_damage_requires_a_complete_buffer_snapshot() {
        let damage = [Damage::Surface(Rectangle::new(
            (1, 2).into(),
            (3, 4).into(),
        ))];
        assert_eq!(
            super::buffer_damage_rects(&damage, 200, 100, 2, Transform::Normal, true),
            vec![crate::metal_renderer::BufferDamage {
                x: 0,
                y: 0,
                width: 200,
                height: 100
            }]
        );
    }

    #[test]
    fn buffer_damage_keeps_buffer_coordinates_with_every_transform_and_viewport() {
        use crate::metal_renderer::BufferDamage;

        let damage = [Damage::Buffer(Rectangle::new(
            (-2, 7).into(),
            (3, 4).into(),
        ))];
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
            for has_viewport in [false, true] {
                assert_eq!(
                    super::buffer_damage_rects(&damage, 200, 100, 2, transform, has_viewport),
                    vec![BufferDamage {
                        x: -2,
                        y: 7,
                        width: 3,
                        height: 4,
                    }],
                    "{transform:?}, viewport={has_viewport}",
                );
                assert!(
                    super::buffer_damage_rects(&[], 200, 100, 2, transform, has_viewport)
                        .is_empty()
                );
            }
        }
    }

    #[test]
    fn mixed_damage_only_converts_surface_coordinates() {
        use crate::metal_renderer::BufferDamage;

        let damage = [
            Damage::Buffer(Rectangle::new((1, 2).into(), (3, 4).into())),
            Damage::Surface(Rectangle::new((5, 6).into(), (7, 8).into())),
        ];
        let buffer = BufferDamage {
            x: 1,
            y: 2,
            width: 3,
            height: 4,
        };
        assert_eq!(
            super::buffer_damage_rects(&damage, 200, 100, 2, Transform::Normal, false),
            vec![
                buffer,
                BufferDamage {
                    x: 10,
                    y: 12,
                    width: 14,
                    height: 16,
                },
            ]
        );
        for (transform, viewport) in [
            (Transform::_90, false),
            (Transform::Normal, true),
            (Transform::Flipped270, true),
        ] {
            assert_eq!(
                super::buffer_damage_rects(&damage, 200, 100, 2, transform, viewport),
                vec![
                    buffer,
                    BufferDamage {
                        x: 0,
                        y: 0,
                        width: 200,
                        height: 100,
                    },
                ]
            );
        }
    }

    #[test]
    fn desktop_is_the_compatible_default() {
        assert_eq!(PresentationMode::parse(None), PresentationMode::Desktop);
        assert_eq!(PresentationMode::parse(Some("")), PresentationMode::Desktop);
        assert_eq!(
            PresentationMode::parse(Some("unknown")),
            PresentationMode::Desktop
        );
    }

    #[test]
    fn rootless_is_case_insensitive() {
        assert_eq!(
            PresentationMode::parse(Some(" ROOTLESS ")),
            PresentationMode::Rootless
        );
    }

    #[test]
    fn startup_maximize_waits_until_rootless_content_is_presented() {
        assert!(!honor_rootless_maximize(false, true));
        assert!(honor_rootless_maximize(true, true));
        assert!(honor_rootless_maximize(false, false));
    }
}
