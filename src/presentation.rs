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

pub fn render_rootless_window(
    rootless: &mut RootlessWindow,
    popups: &[smithay::wayland::shell::xdg::PopupSurface],
) -> usize {
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
        return 0;
    }
    if size.width != rootless.renderer.width || size.height != rootless.renderer.height {
        rootless.renderer.resize(size.width, size.height);
    }
    rootless.renderer.clear(0.08, 0.08, 0.1, 1.0);
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

    if let Err(error) = rootless.renderer.swap_buffers() {
        log::error!("Failed to present rootless window: {error}");
    }
    if let Some((buffer_width, buffer_height)) =
        rootless.renderer.cached_surface_size(&root_surface.id())
    {
        let buffer_scale = smithay::wayland::compositor::with_states(root_surface, |states| {
            states
                .cached_state
                .get::<smithay::wayland::compositor::SurfaceAttributes>()
                .current()
                .buffer_scale
                .max(1)
        });
        let destination_width =
            (f64::from(buffer_width) / f64::from(buffer_scale) * scale).round() as i32;
        let destination_height =
            (f64::from(buffer_height) / f64::from(buffer_scale) * scale).round() as i32;
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
                buffer_scale,
                destination_width,
                destination_height
            );
            rootless.last_render_metrics = Some(metrics);
        }
    }
    rendered
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

pub fn surface_under(
    rootless: &RootlessWindow,
    popups: &[smithay::wayland::shell::xdg::PopupSurface],
    location: smithay::utils::Point<f64, smithay::utils::Logical>,
) -> Option<(
    smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
    smithay::utils::Point<f64, smithay::utils::Logical>,
)> {
    let root_surface = rootless.toplevel.wl_surface();
    for popup in popups.iter().rev() {
        let Some((popup_root, popup_location)) = popup_root_and_location(popup, popups) else {
            continue;
        };
        if &popup_root == root_surface
            && let Some(hit) = surface_tree_hit(
                &rootless.renderer,
                popup.wl_surface(),
                popup_location,
                location,
            )
        {
            return Some(hit);
        }
    }
    let root_origin = toplevel_window_geometry(&rootless.toplevel)
        .map(|geometry| (-geometry.loc.x, -geometry.loc.y).into())
        .unwrap_or_else(|| (0, 0).into());
    surface_tree_hit(&rootless.renderer, root_surface, root_origin, location)
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
    let mut parent = popup.get_parent_surface()?;
    let mut location = popup
        .with_committed_state(|state| state.map(|state| state.geometry.loc).unwrap_or_default());
    loop {
        while let Some(subsurface_parent) = smithay::wayland::compositor::get_parent(&parent) {
            let offset = smithay::wayland::compositor::with_states(&parent, |states| {
                states
                    .cached_state
                    .get::<smithay::wayland::compositor::SubsurfaceCachedState>()
                    .current()
                    .location
            });
            location += offset;
            parent = subsurface_parent;
        }
        let Some(parent_popup) = popups
            .iter()
            .find(|candidate| candidate.wl_surface() == &parent)
        else {
            break;
        };
        location += parent_popup.with_committed_state(|state| {
            state.map(|state| state.geometry.loc).unwrap_or_default()
        });
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
            let viewport_destination = states
                .cached_state
                .get::<smithay::wayland::viewporter::ViewportCachedState>()
                .current()
                .dst;
            let x = (f64::from(location.0 + offset.x) * scale).round() as i32;
            let y = (f64::from(location.1 + offset.y) * scale).round() as i32;
            let surface_id = surface.id();
            let buffer_assignment = current.buffer.take();
            let damage = std::mem::take(&mut current.damage);
            match buffer_assignment {
                Some(BufferAssignment::NewBuffer(buffer)) => {
                    let buffer_scale = current.buffer_scale.max(1);
                    let buffer_id = buffer.id();
                    if crate::render::with_buffer_pixels(
                        &buffer,
                        |width, height, bytes_per_row, pixels, alpha| {
                            let buffer_damage = buffer_damage_rects(
                                &damage,
                                width,
                                height,
                                buffer_scale,
                                current.buffer_transform,
                                viewport_destination.is_some(),
                            );
                            let destination_width = viewport_destination
                                .map(|destination| {
                                    (f64::from(destination.w) * scale).round() as i32
                                })
                                .unwrap_or_else(|| {
                                    (f64::from(width) / f64::from(buffer_scale) * scale).round()
                                        as i32
                                });
                            let destination_height = viewport_destination
                                .map(|destination| {
                                    (f64::from(destination.h) * scale).round() as i32
                                })
                                .unwrap_or_else(|| {
                                    (f64::from(height) / f64::from(buffer_scale) * scale).round()
                                        as i32
                                });
                            renderer.draw_pixels(
                                surface_id.clone(),
                                buffer_id,
                                x,
                                y,
                                destination_width,
                                destination_height,
                                width,
                                height,
                                bytes_per_row,
                                pixels,
                                alpha,
                                &buffer_damage,
                            );
                            rendered += 1;
                        },
                    )
                    .is_none()
                        && renderer.draw_from_cache(&surface_id, x, y, scale, viewport_destination)
                    {
                        rendered += 1;
                    }
                    // Cocoa-Way copies SHM pixels into a Metal texture before
                    // presenting, so the client can reuse this buffer now.
                    buffer.release();
                }
                Some(BufferAssignment::Removed) => renderer.evict_texture(&surface_id),
                None => {
                    if renderer.draw_from_cache(&surface_id, x, y, scale, viewport_destination) {
                        rendered += 1;
                    }
                }
            }
        },
        |_, _, _| true,
    );
    rendered
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
    if transform != Transform::Normal || has_viewport {
        return vec![crate::metal_renderer::BufferDamage {
            x: 0,
            y: 0,
            width,
            height,
        }];
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
    renderer: &crate::metal_renderer::MetalRenderer,
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
            let offset = states
                .cached_state
                .get::<SubsurfaceCachedState>()
                .current()
                .location;
            TraversalAction::DoChildren(*location + offset)
        },
        |surface, states, location| {
            let offset = states
                .cached_state
                .get::<SubsurfaceCachedState>()
                .current()
                .location;
            let surface_location = *location + offset;
            let surface_id = surface.id();
            let Some((buffer_width, buffer_height)) = renderer.cached_surface_size(&surface_id)
            else {
                return;
            };
            let mut attributes = states.cached_state.get::<SurfaceAttributes>();
            let current = attributes.current();
            let viewport_destination = states
                .cached_state
                .get::<smithay::wayland::viewporter::ViewportCachedState>()
                .current()
                .dst;
            let buffer_scale = current.buffer_scale.max(1);
            let width = viewport_destination
                .map(|destination| destination.w)
                .unwrap_or(buffer_width / buffer_scale);
            let height = viewport_destination
                .map(|destination| destination.h)
                .unwrap_or(buffer_height / buffer_scale);
            let local = point - surface_location.to_f64();
            let within_buffer = local.x >= 0.0
                && local.y >= 0.0
                && local.x < f64::from(width)
                && local.y < f64::from(height);
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

#[cfg(test)]
mod tests {
    use super::{PresentationMode, honor_rootless_maximize};

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
