//! One committed surface-to-buffer mapping for drawing and input bounds.

use smithay::reexports::wayland_server::protocol::wl_output::Transform;
use smithay::utils::{Logical, Point, Rectangle, Size};
use smithay::wayland::viewporter::ViewportCachedState;

#[derive(Clone, Copy, Debug)]
pub(crate) struct SurfaceMapping {
    pub buffer_scale: i32,
    pub transform: Transform,
    pub viewport: ViewportCachedState,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct DrawGeometry {
    pub width: i32,
    pub height: i32,
    // Metal triangle strip order: bottom-left, bottom-right, top-left, top-right.
    pub uv: [[f32; 2]; 4],
    // Clamp linear filtering to texel centers inside the crop, not adjacent pixels.
    pub sample_bounds: [f32; 4],
}

impl SurfaceMapping {
    fn source(self, buffer: (i32, i32)) -> Option<(Rectangle<f64, Logical>, Size<f64, Logical>)> {
        if buffer.0 <= 0 || buffer.1 <= 0 {
            return None;
        }
        let transform = smithay::utils::Transform::from(self.transform);
        let scale = f64::from(self.buffer_scale.max(1));
        let area = transform.transform_size(Size::<f64, Logical>::from((
            f64::from(buffer.0),
            f64::from(buffer.1),
        )));
        let area = Size::from((area.w / scale, area.h / scale));
        let src = self.viewport.src.unwrap_or(Rectangle::from_size(area));
        if ![src.loc.x, src.loc.y, src.size.w, src.size.h]
            .into_iter()
            .all(f64::is_finite)
            || src.loc.x < 0.0
            || src.loc.y < 0.0
            || src.size.w <= 0.0
            || src.size.h <= 0.0
            || src.loc.x + src.size.w > area.w
            || src.loc.y + src.size.h > area.h
            || (self.viewport.dst.is_none()
                && self.viewport.src.is_some()
                && (src.size.w.fract() != 0.0 || src.size.h.fract() != 0.0))
        {
            return None;
        }
        Some((src, area))
    }

    pub(crate) fn logical_size(self, buffer: (i32, i32)) -> Option<Size<f64, Logical>> {
        let (source, _) = self.source(buffer)?;
        let size = self.viewport.dst.map_or(source.size, |size| size.to_f64());
        (size.w > 0.0 && size.h > 0.0).then_some(size)
    }

    pub(crate) fn geometry(self, buffer: (i32, i32), output_scale: f64) -> Option<DrawGeometry> {
        if !output_scale.is_finite() || output_scale <= 0.0 {
            return None;
        }
        let (source, area) = self.source(buffer)?;
        let size = self.logical_size(buffer)?;
        let width = (size.w * output_scale).round();
        let height = (size.h * output_scale).round();
        if width < 1.0
            || height < 1.0
            || width > f64::from(i32::MAX)
            || height > f64::from(i32::MAX)
        {
            return None;
        }
        let transform = smithay::utils::Transform::from(self.transform);
        let scale = f64::from(self.buffer_scale.max(1));
        let uv = [[0.0, 1.0], [1.0, 1.0], [0.0, 0.0], [1.0, 0.0]].map(|[x, y]| {
            let point: Point<f64, Logical> = (
                source.loc.x + x * source.size.w,
                source.loc.y + y * source.size.h,
            )
                .into();
            let point = point.to_buffer(scale, transform, &area);
            [
                (point.x / f64::from(buffer.0)) as f32,
                (point.y / f64::from(buffer.1)) as f32,
            ]
        });
        let mut bounds = [
            f32::INFINITY,
            f32::INFINITY,
            f32::NEG_INFINITY,
            f32::NEG_INFINITY,
        ];
        for [u, v] in uv {
            bounds[0] = bounds[0].min(u);
            bounds[1] = bounds[1].min(v);
            bounds[2] = bounds[2].max(u);
            bounds[3] = bounds[3].max(v);
        }
        for (axis, pixels) in [buffer.0, buffer.1].into_iter().enumerate() {
            let midpoint = (bounds[axis] + bounds[axis + 2]) * 0.5;
            let inset = 0.5 / pixels as f32;
            bounds[axis] = (bounds[axis] + inset).min(midpoint);
            bounds[axis + 2] = (bounds[axis + 2] - inset).max(midpoint);
        }
        Some(DrawGeometry {
            width: width as i32,
            height: height as i32,
            uv,
            sample_bounds: bounds,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mapping(transform: Transform) -> SurfaceMapping {
        SurfaceMapping {
            buffer_scale: 2,
            transform,
            viewport: ViewportCachedState {
                src: Some(Rectangle::new((10.0, 5.0).into(), (30.0, 20.0).into())),
                dst: None,
            },
        }
    }

    #[test]
    fn source_only_crop_has_surface_local_size_and_scaled_output() {
        let map = mapping(Transform::Normal);
        assert_eq!(map.logical_size((200, 100)), Some((30.0, 20.0).into()));
        let draw = map.geometry((200, 100), 1.5).unwrap();
        assert_eq!((draw.width, draw.height), (45, 30));
        assert_eq!(draw.uv, [[0.1, 0.5], [0.4, 0.5], [0.1, 0.1], [0.4, 0.1]]);
        assert!((draw.sample_bounds[0] - 0.1025).abs() < 1e-6);
        assert!((draw.sample_bounds[3] - 0.495).abs() < 1e-6);
    }

    #[test]
    fn all_transforms_map_asymmetric_crop_to_buffer_coordinates() {
        let cases = [
            (Transform::Normal, [0.1, 0.1]),
            (Transform::_90, [0.95, 0.2]),
            (Transform::_180, [0.9, 0.9]),
            (Transform::_270, [0.05, 0.8]),
            (Transform::Flipped, [0.9, 0.1]),
            (Transform::Flipped90, [0.05, 0.2]),
            (Transform::Flipped180, [0.1, 0.9]),
            (Transform::Flipped270, [0.95, 0.8]),
        ];
        for (transform, top_left) in cases {
            let draw = mapping(transform).geometry((200, 100), 2.0).unwrap();
            assert_eq!(draw.uv[2], top_left, "{transform:?}");
            assert_eq!((draw.width, draw.height), (60, 40));
            for [u, v] in draw.uv {
                assert!((0.0..=1.0).contains(&u) && (0.0..=1.0).contains(&v));
            }
        }
    }

    #[test]
    fn fractional_crop_with_destination_and_geometry_only_changes() {
        let mut map = mapping(Transform::_90);
        map.viewport.src = Some(Rectangle::new((1.25, 2.5).into(), (12.5, 17.25).into()));
        map.viewport.dst = Some((80, 40).into());
        let draw = map.geometry((200, 100), 1.25).unwrap();
        assert_eq!((draw.width, draw.height), (100, 50));
        assert_eq!(draw.uv[2], [0.975, 0.025]);
        map.viewport = ViewportCachedState::default();
        let draw = map.geometry((200, 100), 1.25).unwrap();
        assert_eq!((draw.width, draw.height), (63, 125));
        assert_eq!(draw.uv, [[0.0, 0.0], [0.0, 1.0], [1.0, 0.0], [1.0, 1.0]]);
    }

    #[test]
    fn invalid_crops_do_not_expand_input_or_sample_outside_the_buffer() {
        for (x, y, w, h) in [
            (90.0, 0.0, 30.0, 20.0),
            (f64::NAN, 0.0, 1.0, 1.0),
            (0.0, -1.0, 1.0, 1.0),
            (0.0, 0.0, 0.0, 1.0),
            (0.0, 0.0, 1.5, 1.0),
        ] {
            let mut map = mapping(Transform::Normal);
            map.viewport.src = Some(Rectangle::new((x, y).into(), (w, h).into()));
            assert!(map.logical_size((200, 100)).is_none());
            assert!(map.geometry((200, 100), 1.0).is_none());
        }
    }
}
