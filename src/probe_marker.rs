//! Decode only the explicit audit client's opaque SHM marker. Not a protocol.

pub(crate) const PIXEL_COUNT: usize = 40;
pub(crate) const PIXEL_BYTES: usize = PIXEL_COUNT * 4;

pub fn decode(width: i32, bgra: &[u8]) -> Option<[u8; 120]> {
    if width < PIXEL_COUNT as i32 || bgra.len() < PIXEL_BYTES {
        return None;
    }
    let mut bytes = [0_u8; 120];
    for (pixel, output) in bgra[..PIXEL_BYTES]
        .chunks_exact(4)
        .zip(bytes.chunks_exact_mut(3))
    {
        if pixel[3] != 255 {
            return None;
        }
        output.copy_from_slice(&[pixel[2], pixel[1], pixel[0]]);
    }
    if &bytes[..4] != b"CWIP"
        || u16::from_le_bytes(bytes[4..6].try_into().ok()?) != 1
        || u16::from_le_bytes(bytes[6..8].try_into().ok()?) != 120
    {
        return None;
    }
    Some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn decode_opaque_rgb_not_memory_byte_order() {
        let mut marker = [0; 120];
        marker[..4].copy_from_slice(b"CWIP");
        marker[4] = 1;
        marker[6] = 120;
        marker[32] = 19;
        let pixels = marker
            .chunks_exact(3)
            .flat_map(|x| [x[2], x[1], x[0], 255])
            .collect::<Vec<_>>();
        assert_eq!(decode(40, &pixels), Some(marker));
        assert!(decode(39, &pixels).is_none());
        assert!(decode(40, &pixels[..159]).is_none());
        assert!(decode(40, &[0; 160]).is_none());
    }

    #[test]
    fn decode_rejects_malformed_headers_and_ignores_trailing_pixels() {
        let mut marker = [0; 120];
        marker[..4].copy_from_slice(b"CWIP");
        marker[4] = 1;
        marker[6] = 120;
        let pixels: Vec<_> = marker
            .chunks_exact(3)
            .flat_map(|rgb| [rgb[2], rgb[1], rgb[0], 255])
            .chain([0; 16])
            .collect();
        assert_eq!(decode(44, &pixels), Some(marker));
        for index in [0, 4, 6] {
            let mut invalid = marker;
            invalid[index] ^= 1;
            let pixels: Vec<_> = invalid
                .chunks_exact(3)
                .flat_map(|rgb| [rgb[2], rgb[1], rgb[0], 255])
                .collect();
            assert!(decode(40, &pixels).is_none());
        }
        assert!(decode(-1, &pixels).is_none());
    }
}
