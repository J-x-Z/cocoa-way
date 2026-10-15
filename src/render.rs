use std::cell::RefCell;

use smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer;
use smithay::reexports::wayland_server::protocol::wl_shm::Format;
use smithay::wayland::shm::{BufferData, with_buffer_contents};

use crate::metal_renderer::TextureAlpha;

// Bound client-controlled copies and retain at most one smaller allocation per thread.
const MAX_SNAPSHOT_BYTES: usize = 256 * 1024 * 1024;
const MAX_CACHED_SNAPSHOT_BYTES: usize = 64 * 1024 * 1024;

thread_local! {
    static PIXEL_SCRATCH: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

struct PixelScratch {
    pixels: Vec<u8>,
}

impl PixelScratch {
    fn take() -> Self {
        Self {
            pixels: PIXEL_SCRATCH.with(|slot| std::mem::take(&mut *slot.borrow_mut())),
        }
    }
}

impl Drop for PixelScratch {
    fn drop(&mut self) {
        if self.pixels.capacity() > MAX_CACHED_SNAPSHOT_BYTES {
            return;
        }
        self.pixels.clear();
        // No TLS borrow survives checkout, including across a reentrant/panicking callback.
        let _ = PIXEL_SCRATCH.try_with(|slot| {
            if let Ok(mut cached) = slot.try_borrow_mut() {
                if self.pixels.capacity() > cached.capacity() {
                    *cached = std::mem::take(&mut self.pixels);
                }
            }
        });
    }
}

/// Read a host-owned snapshot of a Wayland SHM buffer.
///
/// Only the raw copy runs under Smithay's mapping/SIGBUS guard. The callback runs
/// after that guard succeeds, with no reference into client-mutable memory.
/// Invalid, unsupported, over-budget or unallocatable snapshots return `None`.
pub fn with_buffer_pixels<T>(
    buffer: &WlBuffer,
    read: impl FnOnce(i32, i32, usize, &[u8], TextureAlpha) -> T,
) -> Option<T> {
    let mut scratch = PixelScratch::take();
    let (data, layout) = match with_buffer_contents(buffer, |ptr, len, data| {
        // SAFETY: Smithay keeps this raw mapping valid and guards SIGBUS for this copy.
        let layout = unsafe { snapshot_pool_pixels(ptr, len, data, &mut scratch.pixels) }?;
        Some((data, layout))
    }) {
        Ok(result) => result?,
        Err(error) => {
            log::debug!("RENDER: could not read buffer as wl_shm: {}", error);
            return None;
        }
    };
    let (stride, alpha) = prepare_snapshot_pixels(&mut scratch.pixels, layout, data.format)?;
    Some(read(
        data.width,
        data.height,
        stride,
        &scratch.pixels,
        alpha,
    ))
}

#[derive(Clone, Copy, Debug)]
struct PixelLayout {
    offset: usize,
    row_bytes: usize,
    height: usize,
    stride: usize,
    byte_len: usize,
}

impl PixelLayout {
    fn new(
        pool_len: usize,
        offset: usize,
        width: usize,
        height: usize,
        stride: usize,
    ) -> Option<Self> {
        let row_bytes = width.checked_mul(4)?;
        if row_bytes == 0 || stride < row_bytes {
            return None;
        }
        let byte_len = height
            .checked_sub(1)?
            .checked_mul(stride)?
            .checked_add(row_bytes)?;
        let end = offset.checked_add(byte_len)?;
        if end > pool_len || end > isize::MAX as usize || byte_len > MAX_SNAPSHOT_BYTES {
            return None;
        }
        Some(Self {
            offset,
            row_bytes,
            height,
            stride,
            byte_len,
        })
    }
}

/// Copy without ever constructing a reference into the source mapping.
///
/// # Safety
/// For valid supported metadata, `ptr` must address `pool_len` mapped bytes for
/// this call, disjoint from `pixels`. SHM calls must stay inside Smithay's guard.
unsafe fn snapshot_pool_pixels(
    ptr: *const u8,
    pool_len: usize,
    data: BufferData,
    pixels: &mut Vec<u8>,
) -> Option<PixelLayout> {
    shm_format_alpha(data.format)?;
    let layout = PixelLayout::new(
        pool_len,
        usize::try_from(data.offset).ok()?,
        usize::try_from(data.width).ok()?,
        usize::try_from(data.height).ok()?,
        usize::try_from(data.stride).ok()?,
    )?;
    if ptr.is_null() {
        return None;
    }
    pixels.clear();
    pixels.try_reserve_exact(layout.byte_len).ok()?;
    // Only expose initialized host bytes, and only after the raw copy completes.
    unsafe {
        std::ptr::copy_nonoverlapping(ptr.add(layout.offset), pixels.as_mut_ptr(), layout.byte_len);
        pixels.set_len(layout.byte_len);
    }
    Some(layout)
}

fn prepare_snapshot_pixels(
    pixels: &mut Vec<u8>,
    layout: PixelLayout,
    format: Format,
) -> Option<(usize, TextureAlpha)> {
    let alpha = shm_format_alpha(format)?;
    if pixels.len() != layout.byte_len {
        return None;
    }
    // Preserve native BGRA row pitch and the existing opaque-XRGB alpha contract.
    if matches!(format, Format::Argb8888 | Format::Xrgb8888) {
        return Some((layout.stride, alpha));
    }

    let packed_len = layout.row_bytes.checked_mul(layout.height)?;
    for y in 0..layout.height {
        // Compact towards the front; no destination row can overwrite a later source row.
        let src = y * layout.stride;
        let dst = y * layout.row_bytes;
        pixels.copy_within(src..src + layout.row_bytes, dst);
        for pixel in pixels[dst..dst + layout.row_bytes].chunks_exact_mut(4) {
            let bgra = pixel_to_bgra(format, pixel)?;
            pixel.copy_from_slice(&bgra);
        }
    }
    pixels.truncate(packed_len);
    Some((layout.row_bytes, alpha))
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

    fn buffer_data(format: Format) -> BufferData {
        BufferData {
            offset: 0,
            width: 1,
            height: 1,
            stride: 4,
            format,
        }
    }

    fn snapshot_into(pool: &[u8], data: BufferData, pixels: &mut Vec<u8>) -> Option<PixelLayout> {
        // SAFETY: Tests use initialized host memory, disjoint from the destination.
        unsafe { snapshot_pool_pixels(pool.as_ptr(), pool.len(), data, pixels) }
    }

    #[test]
    fn owned_snapshot_preserves_offset_padded_rows_and_survives_source_changes() {
        let source = [
            1, 2, 3, 4, 5, 6, 7, 8, 99, 99, 99, 99, 9, 10, 11, 12, 13, 14, 15, 16,
        ];
        for (format, alpha) in [
            (Format::Argb8888, TextureAlpha::Premultiplied),
            (Format::Xrgb8888, TextureAlpha::Opaque),
        ] {
            let data = BufferData {
                offset: 128,
                width: 2,
                height: 2,
                stride: 12,
                ..buffer_data(format)
            };
            let mut pool = vec![0xEE; data.offset as usize];
            // The last row need not include trailing stride padding.
            pool.extend_from_slice(&source);
            let mut pixels = Vec::new();
            let layout = snapshot_into(&pool, data, &mut pixels).unwrap();
            assert_ne!(pixels.as_ptr(), pool.as_ptr().wrapping_add(128));

            pool.fill(0xDD);
            drop(pool);
            let (stride, detected_alpha) =
                prepare_snapshot_pixels(&mut pixels, layout, format).unwrap();
            assert_eq!(stride, 12);
            assert_eq!(pixels, source);
            assert_eq!(detected_alpha, alpha);
        }
    }

    #[test]
    fn swizzled_snapshot_compacts_padded_rows_from_nonzero_offset() {
        let data = BufferData {
            offset: 5,
            width: 2,
            height: 3,
            stride: 12,
            ..buffer_data(Format::Abgr8888)
        };
        let mut pool = vec![0xEE; 5];
        pool.extend_from_slice(&[
            3, 2, 1, 4, 7, 6, 5, 8, 99, 99, 99, 99, 11, 10, 9, 12, 15, 14, 13, 16, 99, 99, 99, 99,
            19, 18, 17, 20, 23, 22, 21, 24,
        ]);
        let mut pixels = Vec::new();
        let layout = snapshot_into(&pool, data, &mut pixels).unwrap();
        let allocation = pixels.as_ptr();
        let (stride, alpha) = prepare_snapshot_pixels(&mut pixels, layout, data.format).unwrap();

        assert_eq!(stride, 8);
        assert_eq!(alpha, TextureAlpha::Premultiplied);
        assert_eq!(pixels, (1..=24).collect::<Vec<u8>>());
        assert_eq!(pixels.as_ptr(), allocation);
    }

    #[test]
    fn converts_supported_32_bit_formats_to_bgra_without_losing_alpha() {
        let cases = [
            (Format::Argb8888, [1, 2, 3, 4], TextureAlpha::Premultiplied),
            (Format::Xrgb8888, [1, 2, 3, 0xFF], TextureAlpha::Opaque),
            (Format::Abgr8888, [3, 2, 1, 4], TextureAlpha::Premultiplied),
            (Format::Xbgr8888, [3, 2, 1, 0], TextureAlpha::Opaque),
            (Format::Rgba8888, [4, 1, 2, 3], TextureAlpha::Premultiplied),
            (Format::Rgbx8888, [0, 1, 2, 3], TextureAlpha::Opaque),
            (Format::Bgra8888, [4, 3, 2, 1], TextureAlpha::Premultiplied),
            (Format::Bgrx8888, [0, 3, 2, 1], TextureAlpha::Opaque),
        ];

        for (format, source, alpha) in cases {
            let mut pixels = Vec::new();
            let layout = snapshot_into(&source, buffer_data(format), &mut pixels).unwrap();
            let (stride, detected_alpha) =
                prepare_snapshot_pixels(&mut pixels, layout, format).unwrap();
            assert_eq!(stride, 4);
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

    #[test]
    fn native_xrgb_keeps_unused_alpha_bytes_and_reports_opaque() {
        let data = buffer_data(Format::Xrgb8888);
        let mut pixels = Vec::new();
        let layout = snapshot_into(&[1, 2, 3, 0], data, &mut pixels).unwrap();
        assert_eq!(
            prepare_snapshot_pixels(&mut pixels, layout, data.format),
            Some((4, TextureAlpha::Opaque))
        );
        assert_eq!(pixels, [1, 2, 3, 0]);
    }

    #[test]
    fn malformed_metadata_is_rejected_before_copying() {
        let data = buffer_data(Format::Argb8888);
        let invalid = [
            BufferData { offset: -1, ..data },
            BufferData { width: -1, ..data },
            BufferData { width: 0, ..data },
            BufferData { height: -1, ..data },
            BufferData { height: 0, ..data },
            BufferData { stride: -1, ..data },
            BufferData { stride: 0, ..data },
            BufferData { stride: 3, ..data },
            BufferData { width: 2, ..data },
            BufferData { offset: 1, ..data },
            BufferData { height: 2, ..data },
            BufferData {
                height: i32::MAX,
                ..data
            },
            BufferData {
                width: i32::MAX,
                stride: i32::MAX,
                ..data
            },
            BufferData {
                format: Format::Rgb565,
                ..data
            },
        ];
        let mut pixels = vec![42];
        for data in invalid {
            assert!(snapshot_into(&[1, 2, 3, 4], data, &mut pixels).is_none());
            assert_eq!(pixels, [42]);
        }
        assert!(snapshot_into(&[1, 2, 3], data, &mut pixels).is_none());
        // SAFETY: A null source is rejected without dereferencing it.
        assert!(unsafe { snapshot_pool_pixels(std::ptr::null(), 4, data, &mut pixels) }.is_none());
        assert_eq!(pixels, [42]);
    }

    #[test]
    fn layout_rejects_overflow_pointer_limits_and_over_budget_ranges() {
        let max = usize::MAX;
        assert!(PixelLayout::new(max, 0, max / 4 + 1, 1, max).is_none());
        assert!(PixelLayout::new(max, 0, 1, max, 8).is_none());
        assert!(PixelLayout::new(max, 0, 1, 2, max - 1).is_none());
        assert!(PixelLayout::new(max, max - 3, 1, 1, 4).is_none());
        assert!(PixelLayout::new(max, isize::MAX as usize, 1, 1, 4).is_none());
        assert!(PixelLayout::new(MAX_SNAPSHOT_BYTES, 0, 1, MAX_SNAPSHOT_BYTES / 4, 4).is_some());
        assert!(PixelLayout::new(max, 0, 1, MAX_SNAPSHOT_BYTES / 4 + 1, 4).is_none());
    }

    #[test]
    fn snapshot_preparation_rejects_incomplete_host_storage() {
        let layout = PixelLayout::new(4, 0, 1, 1, 4).unwrap();
        assert!(prepare_snapshot_pixels(&mut vec![0; 3], layout, Format::Argb8888).is_none());
        assert!(prepare_snapshot_pixels(&mut vec![0; 4], layout, Format::Rgb565).is_none());
    }

    #[test]
    fn scratch_reuse_does_not_alias_nested_callbacks() {
        PIXEL_SCRATCH.with(|slot| *slot.borrow_mut() = Vec::new());
        let mut outer = PixelScratch::take();
        let data = BufferData {
            width: 2,
            stride: 8,
            ..buffer_data(Format::Argb8888)
        };
        let source = [1, 2, 3, 4, 5, 6, 7, 8];
        snapshot_into(&source, data, &mut outer.pixels).unwrap();
        let allocation = outer.pixels.as_ptr();

        let read = |pixels: &[u8]| {
            let mut inner = PixelScratch::take();
            snapshot_into(&[9; 4], buffer_data(Format::Argb8888), &mut inner.pixels).unwrap();
            assert_ne!(pixels.as_ptr(), inner.pixels.as_ptr());
            assert_eq!(pixels, source);
        };
        read(&outer.pixels);
        drop(outer);

        let mut reused = PixelScratch::take();
        assert!(reused.pixels.is_empty());
        snapshot_into(&[8; 4], buffer_data(Format::Argb8888), &mut reused.pixels).unwrap();
        assert_eq!(reused.pixels.as_ptr(), allocation);
        assert_eq!(reused.pixels, [8; 4]);
    }

    #[test]
    fn scratch_is_released_on_callback_unwind() {
        PIXEL_SCRATCH.with(|slot| *slot.borrow_mut() = Vec::new());
        let result = std::panic::catch_unwind(|| {
            let mut scratch = PixelScratch::take();
            snapshot_into(&[1; 4], buffer_data(Format::Argb8888), &mut scratch.pixels).unwrap();
            panic!("callback failed");
        });
        assert!(result.is_err());
        let scratch = PixelScratch::take();
        assert!(scratch.pixels.is_empty());
        assert!(scratch.pixels.capacity() >= 4);
    }
}
