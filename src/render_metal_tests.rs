//! Opt-in, bounded GPU readback checks. No NSApplication, window, or drawable.
use super::*;
use smithay::reexports::wayland_server::protocol::wl_output::Transform;
use smithay::utils::Rectangle;
use smithay::wayland::viewporter::ViewportCachedState;

fn texture(
    device: &ProtocolObject<dyn MTLDevice>,
    width: usize,
    height: usize,
    target: bool,
) -> Retained<ProtocolObject<dyn MTLTexture>> {
    let desc = unsafe { MTLTextureDescriptor::new() };
    desc.setTextureType(MTLTextureType::MTLTextureType2D);
    desc.setPixelFormat(MTLPixelFormat::BGRA8Unorm);
    if target {
        desc.setStorageMode(MTLStorageMode::Shared);
    }
    unsafe {
        desc.setWidth(width);
        desc.setHeight(height);
    }
    desc.setUsage(if target {
        MTLTextureUsage::RenderTarget
    } else {
        MTLTextureUsage::ShaderRead
    });
    let layout = device.heapTextureSizeAndAlignWithDescriptor(&desc);
    let estimate = texture_reservation_bytes(layout, width as i32, height as i32).unwrap();
    let texture = device
        .newTextureWithDescriptor(&desc)
        .expect("offscreen texture");
    assert!(
        texture.allocatedSize() <= estimate.max(width * height * 4),
        "{width}x{height} target={target}: allocated={} heap_estimate={estimate}",
        texture.allocatedSize()
    );
    texture
}

fn region(width: usize, height: usize) -> MTLRegion {
    MTLRegion {
        origin: MTLOrigin { x: 0, y: 0, z: 0 },
        size: MTLSize {
            width,
            height,
            depth: 1,
        },
    }
}

// Independent surface-edge -> raw-buffer-edge reference (6x8 source pixels).
fn raw_point(transform: Transform, x: f64, y: f64) -> [f64; 2] {
    match transform {
        Transform::Normal => [x, y],
        Transform::_90 => [6.0 - y, x],
        Transform::_180 => [6.0 - x, 8.0 - y],
        Transform::_270 => [y, 8.0 - x],
        Transform::Flipped => [6.0 - x, y],
        Transform::Flipped90 => [y, x],
        Transform::Flipped180 => [x, 8.0 - y],
        Transform::Flipped270 => [6.0 - y, 8.0 - x],
        _ => panic!("unknown test transform"),
    }
}

fn expected_pixel(
    transform: Transform,
    x: usize,
    y: usize,
    width: usize,
    height: usize,
    alpha: TextureAlpha,
    crop: [f64; 4],
) -> [f64; 4] {
    let [left, top, crop_width, crop_height] = crop.map(|value| value * 2.0);
    let mut point = raw_point(
        transform,
        left + (x as f64 + 0.5) * crop_width / width as f64,
        top + (y as f64 + 0.5) * crop_height / height as f64,
    );
    let corners = [
        [left, top],
        [left + crop_width, top],
        [left, top + crop_height],
        [left + crop_width, top + crop_height],
    ]
    .map(|[x, y]| raw_point(transform, x, y));
    for axis in 0..2 {
        let lo = corners
            .iter()
            .map(|p| p[axis])
            .fold(f64::INFINITY, f64::min);
        let hi = corners
            .iter()
            .map(|p| p[axis])
            .fold(f64::NEG_INFINITY, f64::max);
        let center = (lo + hi) * 0.5;
        point[axis] = point[axis].clamp((lo + 0.5).min(center), (hi - 0.5).max(center)) - 0.5;
    }
    // The source ramp is affine, so this is also the exact bilinear reference.
    let mut pixel = [
        10.0 + point[0] * 10.0,
        20.0 + point[1] * 10.0,
        30.0 + point[0] + point[1],
        128.0,
    ];
    if alpha.uses_blending() {
        for (channel, clear) in pixel.iter_mut().zip([51.0, 102.0, 153.0, 255.0]) {
            *channel += clear * (127.0 / 255.0);
        }
    }
    pixel
}

#[test]
#[ignore = "explicit opt-in: requires a Metal device; creates only offscreen textures"]
fn headless_metal_crop_transform_scale_alpha() {
    objc2::rc::autoreleasepool(|_| unsafe {
        let device =
            Retained::<ProtocolObject<dyn MTLDevice>>::from_raw(MTLCreateSystemDefaultDevice())
                .expect("Metal device required for explicit GPU validation");
        eprintln!("Offscreen Metal validation on {:?}", device.name());
        let library = device
            .newLibraryWithSource_options_error(&NSString::from_str(SHADER_SOURCE), None)
            .expect("production shader compiles");
        let vertex = library
            .newFunctionWithName(&NSString::from_str("vert_main"))
            .unwrap();
        let fragment = library
            .newFunctionWithName(&NSString::from_str("frag_blit"))
            .unwrap();
        let opaque = make_pipeline(&device, &vertex, &fragment, false).unwrap();
        let premultiplied = make_pipeline(&device, &vertex, &fragment, true).unwrap();
        let border = library
            .newFunctionWithName(&NSString::from_str("frag_border"))
            .unwrap();
        let _border_pipeline = make_pipeline(&device, &vertex, &border, true).unwrap();
        let queue = device.newCommandQueue().unwrap();
        let source = texture(&device, 6, 8, false);
        let pixels: Vec<u8> = (0..8)
            .flat_map(|y| (0..6).flat_map(move |x| [10 + x * 10, 20 + y * 10, 30 + x + y, 128]))
            .collect();
        source.replaceRegion_mipmapLevel_withBytes_bytesPerRow(
            region(6, 8),
            0,
            slice_bytes(&pixels),
            24,
        );
        let mut cases = 0;
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
            for alpha in [TextureAlpha::Opaque, TextureAlpha::Premultiplied] {
                for scale in [2.0, 3.0] {
                    for (crop, destination) in [
                        ([0.5, 0.5, 1.0, 2.0], None),
                        ([0.5, 0.5, 1.0, 2.0], Some((2, 1).into())),
                        ([0.625, 0.875, 1.25, 1.5], Some((2, 1).into())),
                        ([1.125, 1.375, 0.25, 0.25], Some((2, 2).into())),
                    ] {
                        let mapping = SurfaceMapping {
                            buffer_scale: 2,
                            transform,
                            viewport: ViewportCachedState {
                                src: Some(Rectangle::new(
                                    (crop[0], crop[1]).into(),
                                    (crop[2], crop[3]).into(),
                                )),
                                dst: destination,
                            },
                        };
                        let geometry = mapping.geometry((6, 8), scale).unwrap();
                        let (width, height) = (geometry.width as usize, geometry.height as usize);
                        let target = texture(&device, width, height, true);
                        let pass = MTLRenderPassDescriptor::new();
                        let attachment = pass.colorAttachments().objectAtIndexedSubscript(0);
                        attachment.setTexture(Some(&target));
                        attachment.setLoadAction(MTLLoadAction::Clear);
                        attachment.setStoreAction(MTLStoreAction::Store);
                        attachment.setClearColor(MTLClearColor {
                            red: 0.6,
                            green: 0.4,
                            blue: 0.2,
                            alpha: 1.0,
                        });
                        let command = queue.commandBuffer().unwrap();
                        let encoder = command.renderCommandEncoderWithDescriptor(&pass).unwrap();
                        let rect = RectUniform {
                            x: -1.0,
                            y: -1.0,
                            w: 2.0,
                            h: 2.0,
                        };
                        let uv = UvUniform::from(geometry);
                        encoder.setRenderPipelineState(if alpha.uses_blending() {
                            &premultiplied
                        } else {
                            &opaque
                        });
                        encoder.setVertexBytes_length_atIndex(
                            as_bytes(&rect),
                            std::mem::size_of_val(&rect),
                            0,
                        );
                        encoder.setVertexBytes_length_atIndex(
                            as_bytes(&uv),
                            std::mem::size_of_val(&uv),
                            1,
                        );
                        encoder.setFragmentBytes_length_atIndex(
                            as_bytes(&uv),
                            std::mem::size_of_val(&uv),
                            0,
                        );
                        encoder.setFragmentTexture_atIndex(Some(&source), 0);
                        encoder.drawPrimitives_vertexStart_vertexCount(
                            MTLPrimitiveType::TriangleStrip,
                            0,
                            4,
                        );
                        encoder.endEncoding();
                        command.commit();
                        let deadline =
                            std::time::Instant::now() + std::time::Duration::from_secs(3);
                        while !matches!(
                            command.status(),
                            MTLCommandBufferStatus::Completed | MTLCommandBufferStatus::Error
                        ) {
                            assert!(
                                std::time::Instant::now() < deadline,
                                "bounded Metal validation timed out"
                            );
                            std::thread::sleep(std::time::Duration::from_millis(1));
                        }
                        assert_eq!(command.status(), MTLCommandBufferStatus::Completed);
                        let mut actual = vec![0u8; width * height * 4];
                        target.getBytes_bytesPerRow_fromRegion_mipmapLevel(
                            NonNull::new(actual.as_mut_ptr().cast()).unwrap(),
                            width * 4,
                            region(width, height),
                            0,
                        );
                        for y in 0..height {
                            for x in 0..width {
                                let expected =
                                    expected_pixel(transform, x, y, width, height, alpha, crop);
                                for channel in 0..4 {
                                    let actual = actual[(y * width + x) * 4 + channel];
                                    assert!(
                                        (f64::from(actual) - expected[channel]).abs() <= 1.1,
                                        "{transform:?} {alpha:?} scale={scale} dst={destination:?} ({x},{y}) channel={channel}: {actual} != {}",
                                        expected[channel]
                                    );
                                }
                            }
                        }
                        actual.clear();
                        cases += 1;
                    }
                }
            }
        }
        eprintln!("Validated {cases} offscreen crop/transform/scale/alpha cases");
    });
}

#[test]
#[ignore = "explicit opt-in: 4K offscreen Metal texture and fractional crop readback"]
fn headless_metal_4k_fractional_crop() {
    objc2::rc::autoreleasepool(|_| unsafe {
        let device =
            Retained::<ProtocolObject<dyn MTLDevice>>::from_raw(MTLCreateSystemDefaultDevice())
                .expect("Metal device");
        let library = device
            .newLibraryWithSource_options_error(&NSString::from_str(SHADER_SOURCE), None)
            .unwrap();
        let vertex = library
            .newFunctionWithName(&NSString::from_str("vert_main"))
            .unwrap();
        let fragment = library
            .newFunctionWithName(&NSString::from_str("frag_blit"))
            .unwrap();
        let pipeline = make_pipeline(&device, &vertex, &fragment, false).unwrap();
        let queue = device.newCommandQueue().unwrap();
        let source = texture(&device, 3840, 2160, false);
        let pixel =
            |x: usize, y: usize| [(x % 251) as u8, (y % 241) as u8, ((x + y) % 127) as u8, 128];
        let pixels: Vec<_> = (0..2160)
            .flat_map(|y| (0..3840).flat_map(move |x| pixel(x, y)))
            .collect();
        source.replaceRegion_mipmapLevel_withBytes_bytesPerRow(
            region(3840, 2160),
            0,
            slice_bytes(&pixels),
            3840 * 4,
        );
        let crop = Rectangle::new((123.25, 456.5).into(), (32.5, 24.25).into());
        let mapping = SurfaceMapping {
            buffer_scale: 2,
            transform: Transform::Normal,
            viewport: ViewportCachedState {
                src: Some(crop),
                dst: Some((43, 29).into()),
            },
        };
        let geometry = mapping.geometry((3840, 2160), 1.5).unwrap();
        assert_eq!((geometry.width, geometry.height), (65, 44));
        let (width, height) = (geometry.width as usize, geometry.height as usize);
        let target = texture(&device, width, height, true);
        let pass = MTLRenderPassDescriptor::new();
        let attachment = pass.colorAttachments().objectAtIndexedSubscript(0);
        attachment.setTexture(Some(&target));
        attachment.setLoadAction(MTLLoadAction::Clear);
        attachment.setStoreAction(MTLStoreAction::Store);
        let command = queue.commandBuffer().unwrap();
        let encoder = command.renderCommandEncoderWithDescriptor(&pass).unwrap();
        let rect = RectUniform {
            x: -1.0,
            y: -1.0,
            w: 2.0,
            h: 2.0,
        };
        let uv = UvUniform::from(geometry);
        encoder.setRenderPipelineState(&pipeline);
        encoder.setVertexBytes_length_atIndex(as_bytes(&rect), std::mem::size_of_val(&rect), 0);
        encoder.setVertexBytes_length_atIndex(as_bytes(&uv), std::mem::size_of_val(&uv), 1);
        encoder.setFragmentBytes_length_atIndex(as_bytes(&uv), std::mem::size_of_val(&uv), 0);
        encoder.setFragmentTexture_atIndex(Some(&source), 0);
        encoder.drawPrimitives_vertexStart_vertexCount(MTLPrimitiveType::TriangleStrip, 0, 4);
        encoder.endEncoding();
        command.commit();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while !matches!(
            command.status(),
            MTLCommandBufferStatus::Completed | MTLCommandBufferStatus::Error
        ) {
            assert!(
                std::time::Instant::now() < deadline,
                "bounded 4K Metal validation timed out"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert_eq!(command.status(), MTLCommandBufferStatus::Completed);
        let mut actual = vec![0u8; width * height * 4];
        target.getBytes_bytesPerRow_fromRegion_mipmapLevel(
            NonNull::new(actual.as_mut_ptr().cast()).unwrap(),
            width * 4,
            region(width, height),
            0,
        );
        for y in 0..height {
            for x in 0..width {
                let sx =
                    ((crop.loc.x + (x as f64 + 0.5) * crop.size.w / width as f64) * 2.0).clamp(
                        crop.loc.x * 2.0 + 0.5,
                        (crop.loc.x + crop.size.w) * 2.0 - 0.5,
                    ) - 0.5;
                let sy =
                    ((crop.loc.y + (y as f64 + 0.5) * crop.size.h / height as f64) * 2.0).clamp(
                        crop.loc.y * 2.0 + 0.5,
                        (crop.loc.y + crop.size.h) * 2.0 - 0.5,
                    ) - 0.5;
                let (ix, iy) = (sx.floor() as usize, sy.floor() as usize);
                let (fx, fy) = (sx.fract(), sy.fract());
                for channel in 0..4 {
                    let top = f64::from(pixel(ix, iy)[channel]) * (1.0 - fx)
                        + f64::from(pixel(ix + 1, iy)[channel]) * fx;
                    let bottom = f64::from(pixel(ix, iy + 1)[channel]) * (1.0 - fx)
                        + f64::from(pixel(ix + 1, iy + 1)[channel]) * fx;
                    let expected = top * (1.0 - fy) + bottom * fy;
                    assert!(
                        (f64::from(actual[(y * width + x) * 4 + channel]) - expected).abs() < 1.6,
                        "4K fractional crop ({x},{y}) channel={channel}"
                    );
                }
            }
        }
        eprintln!(
            "Validated 3840x2160 source, buffer scale=2, fractional source/destination, output scale=1.5, 65x44 readback"
        );
    });
}
