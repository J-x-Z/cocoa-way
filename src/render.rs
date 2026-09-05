use smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer;
use smithay::reexports::wayland_server::protocol::wl_shm::Format;
use smithay::wayland::shm::with_buffer_contents;

use crate::metal_renderer::TextureAlpha;

/// Read a Wayland SHM buffer while its pool mapping is valid.
///
/// Formats already laid out as BGRA bytes are passed through without allocating.
/// Swizzled formats retain the conversion fallback used by the old render path.
pub fn with_buffer_pixels<T>(
    buffer: &WlBuffer,
    read: impl FnOnce(i32, i32, usize, &[u8], TextureAlpha) -> T,
) -> Option<T> {
    match with_buffer_contents(buffer, |ptr, len, data| {
        if data.width <= 0
            || data.height <= 0
            || data.stride <= 0
            || data.offset < 0
            || data.stride < data.width.saturating_mul(4)
        {
            return None;
        }

        let offset = data.offset as usize;
        let stride = data.stride as usize;
        let row_bytes = data.width as usize * 4;
        let required = offset
            .checked_add((data.height as usize - 1).checked_mul(stride)?)?
            .checked_add(row_bytes)?;
        if required > len {
            return None;
        }

        let slice = unsafe { std::slice::from_raw_parts(ptr, len) };
        match data.format {
            // ARGB/XRGB are native BGRA byte rows on little-endian hosts.
            Format::Argb8888 => Some(read(
                data.width,
                data.height,
                stride,
                &slice[offset..required],
                TextureAlpha::Premultiplied,
            )),
            Format::Xrgb8888 => Some(read(
                data.width,
                data.height,
                stride,
                &slice[offset..required],
                TextureAlpha::Opaque,
            )),
            _ => {
                let (_, _, pixels, alpha) = copy_pool_pixels_to_bgra(
                    slice,
                    data.offset,
                    data.width,
                    data.height,
                    data.stride,
                    data.format,
                )?;
                Some(read(data.width, data.height, row_bytes, &pixels, alpha))
            }
        }
    }) {
        Ok(result) => result,
        Err(error) => {
            log::debug!("RENDER: could not read buffer as wl_shm: {}", error);
            None
        }
    }
}

fn copy_pool_pixels_to_bgra(
    slice: &[u8],
    offset: i32,
    width: i32,
    height: i32,
    stride: i32,
    format: Format,
) -> Option<(i32, i32, Vec<u8>, TextureAlpha)> {
    if width <= 0 || height <= 0 || stride <= 0 || offset < 0 {
        return None;
    }
    log::debug!(
        "get_buffer_pixels: {:?}  {}x{}  stride={} offset={}",
        format,
        width,
        height,
        stride,
        offset
    );
    let mut pixels = vec![0u8; (width * height * 4) as usize];

    for y in 0..height {
        let src_base = (offset + y * stride) as usize;
        let dst_base = (y * width * 4) as usize;
        for x in 0..width as usize {
            let s = src_base + x * 4;
            let d = dst_base + x * 4;
            if s + 4 > slice.len() {
                continue;
            }
            let Some([b, g, r, a]) = pixel_to_bgra(format, &slice[s..s + 4]) else {
                return None;
            };
            pixels[d] = b;
            pixels[d + 1] = g;
            pixels[d + 2] = r;
            pixels[d + 3] = a;
        }
    }
    Some((width, height, pixels, shm_format_alpha(format)?))
}

fn shm_format_alpha(format: Format) -> Option<TextureAlpha> {
    match format {
        Format::Argb8888 | Format::Abgr8888 | Format::Rgba8888 | Format::Bgra8888 => {
            Some(TextureAlpha::Premultiplied)
        }
        Format::Xrgb8888 | Format::Xbgr8888 | Format::Rgbx8888 | Format::Bgrx8888 => {
            Some(TextureAlpha::Opaque)
        }
        _ => None,
    }
}

fn pixel_to_bgra(format: Format, bytes: &[u8]) -> Option<[u8; 4]> {
    let b0 = bytes[0];
    let b1 = bytes[1];
    let b2 = bytes[2];
    match format {
        Format::Argb8888 => Some([b0, b1, b2, bytes[3]]),
        Format::Xrgb8888 => Some([b0, b1, b2, 0xFF]),
        Format::Abgr8888 => Some([b2, b1, b0, bytes[3]]),
        Format::Xbgr8888 => Some([b2, b1, b0, 0xFF]),
        Format::Rgba8888 => Some([b1, b2, bytes[3], b0]),
        Format::Rgbx8888 => Some([b1, b2, bytes[3], 0xFF]),
        Format::Bgra8888 => Some([bytes[3], b2, b1, b0]),
        Format::Bgrx8888 => Some([bytes[3], b2, b1, 0xFF]),
        _ => {
            log::debug!("RENDER: unsupported wl_shm format {:?}", format);
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_pool_pixels_applies_non_zero_buffer_offset() {
        let offset = 128;
        let width = 2;
        let height = 1;
        let stride = 8;
        let mut pool = vec![0u8; offset as usize + stride as usize];
        pool[offset as usize..offset as usize + 8].copy_from_slice(&[1, 2, 3, 0, 4, 5, 6, 0]);

        let (_, _, pixels, alpha) =
            copy_pool_pixels_to_bgra(&pool, offset, width, height, stride, Format::Xrgb8888)
                .expect("offset shm buffer should be readable");

        assert_eq!(pixels, vec![1, 2, 3, 0xFF, 4, 5, 6, 0xFF]);
        assert_eq!(alpha, TextureAlpha::Opaque);
    }

    #[test]
    fn converts_supported_32_bit_formats_to_bgra_without_losing_alpha() {
        let cases = [
            (Format::Argb8888, [1, 2, 3, 4], TextureAlpha::Premultiplied),
            (Format::Xrgb8888, [1, 2, 3, 0], TextureAlpha::Opaque),
            (Format::Abgr8888, [3, 2, 1, 4], TextureAlpha::Premultiplied),
            (Format::Xbgr8888, [3, 2, 1, 0], TextureAlpha::Opaque),
            (Format::Rgba8888, [4, 1, 2, 3], TextureAlpha::Premultiplied),
            (Format::Rgbx8888, [0, 1, 2, 3], TextureAlpha::Opaque),
            (Format::Bgra8888, [4, 3, 2, 1], TextureAlpha::Premultiplied),
            (Format::Bgrx8888, [0, 3, 2, 1], TextureAlpha::Opaque),
        ];

        for (format, source, alpha) in cases {
            let (_, _, pixels, detected_alpha) =
                copy_pool_pixels_to_bgra(&source, 0, 1, 1, 4, format).unwrap();
            assert_eq!(
                pixels,
                [
                    1,
                    2,
                    3,
                    if alpha == TextureAlpha::Opaque {
                        0xFF
                    } else {
                        4
                    }
                ]
            );
            assert_eq!(detected_alpha, alpha);
        }
    }
}
