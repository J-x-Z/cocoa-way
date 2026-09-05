use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_foundation::{NSError, NSString};
use objc2_metal::*;
use objc2_quartz_core::{CAMetalDrawable, CAMetalLayer};
use smithay::reexports::wayland_server::backend::ObjectId;
use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;
use std::ptr::NonNull;
use winit::window::Window;

// All three shaders share one vertex function; each has its own fragment function.
const SHADER_SOURCE: &str = r#"
#include <metal_stdlib>
using namespace metal;

struct VertexOut {
    float4 position [[position]];
    float2 texcoord;
};

struct Rect { float x, y, w, h; };

vertex VertexOut vert_main(uint vid [[vertex_id]],
                           constant Rect& rect [[buffer(0)]]) {
    // Z-pattern (BL, BR, TL, TR) — the N-pattern (BL,BR,TR,TL) leaves
    // a left-diamond region uncovered between the two triangles.
    float2 pos[4] = {float2(0,0), float2(1,0), float2(0,1), float2(1,1)};
    float2 uv[4]  = {float2(0,1), float2(1,1), float2(0,0), float2(1,0)};
    VertexOut out;
    out.position = float4(rect.x + pos[vid].x * rect.w,
                          rect.y + pos[vid].y * rect.h, 0.0, 1.0);
    out.texcoord = uv[vid];
    return out;
}

fragment float4 frag_blit(VertexOut in [[stage_in]],
                          texture2d<float> tex [[texture(0)]]) {
    constexpr sampler s(filter::linear, address::clamp_to_edge);
    return tex.sample(s, in.texcoord);
}

fragment float4 frag_border(VertexOut in [[stage_in]],
                             constant float4& color [[buffer(1)]],
                             constant float&  width [[buffer(2)]]) {
    float d = min(min(in.texcoord.x, 1.0-in.texcoord.x),
                  min(in.texcoord.y, 1.0-in.texcoord.y));
    if (d < width) return color;
    discard_fragment();
    return float4(0);
}

"#;

// Rect packed as four floats for the vertex buffer (NDC space)
#[repr(C)]
struct RectUniform {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

// Safety: caller ensures the reference outlives the GPU call.
#[inline(always)]
unsafe fn as_bytes<T>(v: &T) -> NonNull<c_void> {
    unsafe { NonNull::new_unchecked(v as *const T as *mut c_void) }
}
#[inline(always)]
unsafe fn slice_bytes<T>(v: &[T]) -> NonNull<c_void> {
    unsafe { NonNull::new_unchecked(v.as_ptr() as *mut c_void) }
}

struct FrameState {
    drawable: Retained<ProtocolObject<dyn CAMetalDrawable>>,
    command_buffer: Retained<ProtocolObject<dyn MTLCommandBuffer>>,
    encoder: Retained<ProtocolObject<dyn MTLRenderCommandEncoder>>,
}

// Per-surface texture cache entry
struct CachedTexture {
    texture: Retained<ProtocolObject<dyn MTLTexture>>,
    buffer_id: ObjectId,
    alpha: TextureAlpha,
    tex_w: i32,
    tex_h: i32,
    dest_w: i32,
    dest_h: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TextureAlpha {
    Opaque,
    Premultiplied,
}

impl TextureAlpha {
    fn uses_blending(self) -> bool {
        self == Self::Premultiplied
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BufferDamage {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

pub struct MetalRenderer {
    pub window: Window,
    pub width: u32,
    pub height: u32,
    device: Retained<ProtocolObject<dyn MTLDevice>>,
    layer: Retained<CAMetalLayer>,
    command_queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
    opaque_blit_pipeline: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
    alpha_blit_pipeline: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
    border_pipeline: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
    // Set for the duration of one rendered frame
    frame: RefCell<Option<FrameState>>,
    // surface ObjectId → cached GPU texture
    texture_cache: RefCell<HashMap<ObjectId, CachedTexture>>,
}

impl MetalRenderer {
    pub fn new(window: Window) -> Result<Self, String> {
        unsafe {
            // ── 1. Get Metal device ──────────────────────────────────────────
            let device_ptr = MTLCreateSystemDefaultDevice();
            let device: Retained<ProtocolObject<dyn MTLDevice>> =
                Retained::from_raw(device_ptr).ok_or("No Metal device available")?;
            log::info!("Metal device: {:?}", device.name());

            // ── 2. Create CAMetalLayer and attach to the NSView ──────────────
            let layer = CAMetalLayer::new();
            layer.setDevice(Some(&device));
            layer.setPixelFormat(MTLPixelFormat::BGRA8Unorm);
            layer.setFramebufferOnly(false); // needed for texture reads if required
            let scale = window.scale_factor();
            let () = objc2::msg_send![&layer, setContentsScale: scale as f64];

            // Attach the Metal layer to the window's NSView using raw ObjC sends.
            // winit 0.29 on macOS exposes an AppKitWindowHandle { ns_view }.
            use raw_window_handle::{HasWindowHandle, RawWindowHandle};
            let ns_view: *mut objc2::runtime::AnyObject =
                match window.window_handle().map_err(|e| e.to_string())?.as_raw() {
                    RawWindowHandle::AppKit(h) => h.ns_view.as_ptr() as *mut _,
                    _ => return Err("Non-AppKit window handle".into()),
                };
            let () = objc2::msg_send![ns_view, setWantsLayer: true];
            let () = objc2::msg_send![ns_view, setLayer: &*layer];

            let size = window.inner_size();
            let cg_size = objc2_foundation::CGSize {
                width: size.width as f64,
                height: size.height as f64,
            };
            layer.setDrawableSize(cg_size);

            // ── 3. Command queue ─────────────────────────────────────────────
            let command_queue = device
                .newCommandQueue()
                .ok_or("Failed to create command queue")?;

            // ── 4. Compile shaders ───────────────────────────────────────────
            let src = NSString::from_str(SHADER_SOURCE);
            let library = device
                .newLibraryWithSource_options_error(&src, None)
                .map_err(|e: Retained<NSError>| {
                    format!("Shader compile error: {}", e.localizedDescription())
                })?;

            let vert = library
                .newFunctionWithName(&NSString::from_str("vert_main"))
                .ok_or("Missing vert_main")?;
            let frag_blit = library
                .newFunctionWithName(&NSString::from_str("frag_blit"))
                .ok_or("Missing frag_blit")?;
            let frag_border = library
                .newFunctionWithName(&NSString::from_str("frag_border"))
                .ok_or("Missing frag_border")?;

            // ── 5. Pipeline states ───────────────────────────────────────────
            let opaque_blit_pipeline = make_pipeline(&device, &vert, &frag_blit, false)?;
            let alpha_blit_pipeline = make_pipeline(&device, &vert, &frag_blit, true)?;
            let border_pipeline = make_pipeline(&device, &vert, &frag_border, true)?;

            Ok(Self {
                width: size.width,
                height: size.height,
                window,
                device,
                layer,
                command_queue,
                opaque_blit_pipeline,
                alpha_blit_pipeline,
                border_pipeline,
                frame: RefCell::new(None),
                texture_cache: RefCell::new(HashMap::new()),
            })
        }
    }

    pub fn set_scale_factor(&mut self, scale: f64) {
        unsafe {
            let () = objc2::msg_send![&self.layer, setContentsScale: scale];
        }
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        self.width = width;
        self.height = height;
        unsafe {
            self.layer.setDrawableSize(objc2_foundation::CGSize {
                width: width as f64,
                height: height as f64,
            });
        }
    }

    /// Begin a frame: set clear colour, acquire drawable, start the render pass.
    pub fn clear(&self, r: f32, g: f32, b: f32, a: f32) {
        let mut frame_slot = self.frame.borrow_mut();
        // Drop any leftover frame state from a previous incomplete frame.
        *frame_slot = None;

        let Some(drawable) = (unsafe { self.layer.nextDrawable() }) else {
            log::warn!("Metal: nextDrawable returned nil");
            return;
        };
        let Some(cmd_buf) = self.command_queue.commandBuffer() else {
            log::warn!("Metal: commandBuffer returned nil");
            return;
        };

        let rp = unsafe {
            let desc = MTLRenderPassDescriptor::new();
            let ca = desc.colorAttachments().objectAtIndexedSubscript(0);
            let texture = drawable.texture();
            ca.setTexture(Some(&texture));
            ca.setLoadAction(MTLLoadAction::Clear);
            ca.setStoreAction(MTLStoreAction::Store);
            ca.setClearColor(MTLClearColor {
                red: r as f64,
                green: g as f64,
                blue: b as f64,
                alpha: a as f64,
            });
            desc
        };

        let Some(encoder) = cmd_buf.renderCommandEncoderWithDescriptor(&rp) else {
            log::warn!("Metal: renderCommandEncoder returned nil");
            return;
        };

        *frame_slot = Some(FrameState {
            drawable,
            command_buffer: cmd_buf,
            encoder,
        });
    }

    pub fn swap_buffers(&self) -> Result<(), String> {
        let mut frame_slot = self.frame.borrow_mut();
        let Some(frame) = frame_slot.take() else {
            return Ok(());
        };
        unsafe {
            frame.encoder.endEncoding();
            // CAMetalDrawable implements MTLDrawable; cast via raw ObjC message send.
            let () = objc2::msg_send![&*frame.command_buffer, presentDrawable: &*frame.drawable];
            frame.command_buffer.commit();
        }
        Ok(())
    }

    pub fn request_redraw(&self) {
        self.window.request_redraw();
    }

    // ── Texture cache helpers ─────────────────────────────────────────────────

    pub fn evict_texture(&self, surface_id: &ObjectId) {
        self.texture_cache.borrow_mut().remove(surface_id);
    }

    pub fn cached_surface_size(&self, surface_id: &ObjectId) -> Option<(i32, i32)> {
        self.texture_cache
            .borrow()
            .get(surface_id)
            .map(|entry| (entry.tex_w, entry.tex_h))
    }

    /// Render from cache using only surf_id — no buf_id required.
    /// Returns true if a cached texture was found and drawn.
    pub fn draw_from_cache(
        &self,
        surface_id: &ObjectId,
        phys_x: i32,
        phys_y: i32,
        scale: f64,
        viewport_dst: Option<smithay::utils::Size<i32, smithay::utils::Logical>>,
    ) -> bool {
        let cache = self.texture_cache.borrow();
        let Some(entry) = cache.get(surface_id) else {
            return false;
        };
        let dest_w = viewport_dst
            .map(|d| (d.w as f64 * scale).round() as i32)
            .unwrap_or(entry.dest_w);
        let dest_h = viewport_dst
            .map(|d| (d.h as f64 * scale).round() as i32)
            .unwrap_or(entry.dest_h);
        if dest_w <= 0 || dest_h <= 0 {
            return false;
        }
        let frame_ref = self.frame.borrow();
        let Some(frame) = frame_ref.as_ref() else {
            return false;
        };
        let rect = self.to_ndc(phys_x, phys_y, dest_w, dest_h);
        unsafe {
            let enc = &frame.encoder;
            let pipeline = if entry.alpha.uses_blending() {
                &self.alpha_blit_pipeline
            } else {
                &self.opaque_blit_pipeline
            };
            enc.setRenderPipelineState(pipeline);
            enc.setVertexBytes_length_atIndex(
                as_bytes(&rect),
                std::mem::size_of::<RectUniform>(),
                0,
            );
            enc.setFragmentTexture_atIndex(Some(&entry.texture), 0);
            enc.drawPrimitives_vertexStart_vertexCount(MTLPrimitiveType::TriangleStrip, 0, 4);
        }
        true
    }

    // ── Draw calls ───────────────────────────────────────────────────────────

    /// Draw a surface buffer. Pass `tex_w=0, pixels=&[]` on a cache hit.
    pub fn draw_pixels(
        &self,
        surface_id: ObjectId,
        buffer_id: ObjectId,
        x: i32,
        y: i32,
        dest_w: i32,
        dest_h: i32,
        tex_w: i32,
        tex_h: i32,
        bytes_per_row: usize,
        pixels: &[u8],
        alpha: TextureAlpha,
        damage: &[BufferDamage],
    ) {
        if dest_w <= 0 || dest_h <= 0 {
            return;
        }
        let frame_ref = self.frame.borrow();
        let Some(frame) = frame_ref.as_ref() else {
            return;
        };

        let texture = self.get_or_create_texture(
            surface_id,
            buffer_id,
            dest_w,
            dest_h,
            tex_w,
            tex_h,
            bytes_per_row,
            pixels,
            alpha,
            damage,
        );
        let Some(texture) = texture else {
            return;
        };

        let rect = self.to_ndc(x, y, dest_w, dest_h);
        unsafe {
            let enc = &frame.encoder;
            let pipeline = if alpha.uses_blending() {
                &self.alpha_blit_pipeline
            } else {
                &self.opaque_blit_pipeline
            };
            enc.setRenderPipelineState(pipeline);
            enc.setVertexBytes_length_atIndex(
                as_bytes(&rect),
                std::mem::size_of::<RectUniform>(),
                0,
            );
            enc.setFragmentTexture_atIndex(Some(&texture), 0);
            enc.drawPrimitives_vertexStart_vertexCount(MTLPrimitiveType::TriangleStrip, 0, 4);
        }
    }

    pub fn draw_border(&self, x: i32, y: i32, width: i32, height: i32, border_width: f32) {
        if width <= 0 || height <= 0 {
            return;
        }
        let frame_ref = self.frame.borrow();
        let Some(frame) = frame_ref.as_ref() else {
            return;
        };
        let rect = self.to_ndc(x, y, width, height);
        let color: [f32; 4] = [0.0, 0.6, 1.0, 1.0];
        let bw = border_width / width as f32;
        unsafe {
            let enc = &frame.encoder;
            enc.setRenderPipelineState(&self.border_pipeline);
            enc.setVertexBytes_length_atIndex(
                as_bytes(&rect),
                std::mem::size_of::<RectUniform>(),
                0,
            );
            enc.setFragmentBytes_length_atIndex(
                slice_bytes(&color),
                std::mem::size_of_val(&color),
                1,
            );
            enc.setFragmentBytes_length_atIndex(as_bytes(&bw), std::mem::size_of::<f32>(), 2);
            enc.drawPrimitives_vertexStart_vertexCount(MTLPrimitiveType::TriangleStrip, 0, 4);
        }
    }

    fn to_ndc(&self, x: i32, y: i32, w: i32, h: i32) -> RectUniform {
        let fw = self.width as f32;
        let fh = self.height as f32;
        RectUniform {
            x: (2.0 * x as f32 / fw) - 1.0,
            y: 1.0 - (2.0 * (y + h) as f32 / fh),
            w: 2.0 * w as f32 / fw,
            h: 2.0 * h as f32 / fh,
        }
    }

    fn get_or_create_texture(
        &self,
        surface_id: ObjectId,
        buffer_id: ObjectId,
        dest_w: i32,
        dest_h: i32,
        tex_w: i32,
        tex_h: i32,
        bytes_per_row: usize,
        pixels: &[u8],
        alpha: TextureAlpha,
        damage: &[BufferDamage],
    ) -> Option<Retained<ProtocolObject<dyn MTLTexture>>> {
        let mut cache = self.texture_cache.borrow_mut();

        let row_bytes = (tex_w.max(0) as usize).saturating_mul(4);
        let required = (tex_h.max(0) as usize)
            .checked_sub(1)
            .and_then(|rows| rows.checked_mul(bytes_per_row))
            .and_then(|prefix| prefix.checked_add(row_bytes));
        if tex_w <= 0
            || tex_h <= 0
            || bytes_per_row < row_bytes
            || required.is_none_or(|required| pixels.len() < required)
        {
            cache.remove(&surface_id);
            return None;
        }

        let region = MTLRegion {
            origin: MTLOrigin { x: 0, y: 0, z: 0 },
            size: MTLSize {
                width: tex_w as usize,
                height: tex_h as usize,
                depth: 1,
            },
        };

        // Reuse existing texture if same size — avoids VRAM allocation every frame.
        if let Some(entry) = cache.get_mut(&surface_id) {
            if entry.tex_w == tex_w && entry.tex_h == tex_h {
                if let Some(damage) = coalesce_damage(damage, tex_w, tex_h) {
                    let byte_offset = (damage.y as usize)
                        .saturating_mul(bytes_per_row)
                        .saturating_add((damage.x as usize).saturating_mul(4));
                    let damage_region = MTLRegion {
                        origin: MTLOrigin {
                            x: damage.x as usize,
                            y: damage.y as usize,
                            z: 0,
                        },
                        size: MTLSize {
                            width: damage.width as usize,
                            height: damage.height as usize,
                            depth: 1,
                        },
                    };
                    unsafe {
                        entry
                            .texture
                            .replaceRegion_mipmapLevel_withBytes_bytesPerRow(
                                damage_region,
                                0,
                                NonNull::new_unchecked(pixels.as_ptr().add(byte_offset) as *mut _),
                                bytes_per_row,
                            );
                    }
                }
                entry.buffer_id = buffer_id;
                entry.alpha = alpha;
                entry.dest_w = dest_w;
                entry.dest_h = dest_h;
                return Some(entry.texture.clone());
            }
        }

        // First frame or size changed: allocate a new MTLTexture.
        let texture = unsafe {
            let desc = MTLTextureDescriptor::new();
            desc.setTextureType(MTLTextureType::MTLTextureType2D);
            desc.setPixelFormat(MTLPixelFormat::BGRA8Unorm);
            desc.setWidth(tex_w as usize);
            desc.setHeight(tex_h as usize);
            desc.setUsage(MTLTextureUsage::ShaderRead);
            let tex = self.device.newTextureWithDescriptor(&desc)?;
            tex.replaceRegion_mipmapLevel_withBytes_bytesPerRow(
                region,
                0,
                slice_bytes(pixels),
                bytes_per_row,
            );
            tex
        };

        let cloned = texture.clone();
        cache.insert(
            surface_id,
            CachedTexture {
                texture,
                buffer_id,
                alpha,
                tex_w,
                tex_h,
                dest_w,
                dest_h,
            },
        );
        Some(cloned)
    }
}

fn coalesce_damage(damage: &[BufferDamage], width: i32, height: i32) -> Option<BufferDamage> {
    let mut bounds: Option<(i32, i32, i32, i32)> = None;
    for rect in damage {
        let x1 = rect.x.clamp(0, width);
        let y1 = rect.y.clamp(0, height);
        let x2 = rect.x.saturating_add(rect.width).clamp(0, width);
        let y2 = rect.y.saturating_add(rect.height).clamp(0, height);
        if x2 <= x1 || y2 <= y1 {
            continue;
        }
        bounds = Some(match bounds {
            Some((left, top, right, bottom)) => {
                (left.min(x1), top.min(y1), right.max(x2), bottom.max(y2))
            }
            None => (x1, y1, x2, y2),
        });
    }
    bounds.map(|(x1, y1, x2, y2)| BufferDamage {
        x: x1,
        y: y1,
        width: x2 - x1,
        height: y2 - y1,
    })
}

#[cfg(test)]
mod tests {
    use super::{BufferDamage, TextureAlpha, coalesce_damage};

    #[test]
    fn damage_is_clipped_and_coalesced() {
        let damage = [
            BufferDamage {
                x: -5,
                y: 10,
                width: 20,
                height: 15,
            },
            BufferDamage {
                x: 80,
                y: 70,
                width: 40,
                height: 40,
            },
        ];
        assert_eq!(
            coalesce_damage(&damage, 100, 90),
            Some(BufferDamage {
                x: 0,
                y: 10,
                width: 100,
                height: 80,
            })
        );
    }

    #[test]
    fn empty_damage_skips_texture_upload() {
        assert_eq!(coalesce_damage(&[], 100, 90), None);
    }

    #[test]
    fn only_premultiplied_textures_use_the_alpha_pipeline() {
        assert!(!TextureAlpha::Opaque.uses_blending());
        assert!(TextureAlpha::Premultiplied.uses_blending());
    }
}

unsafe fn make_pipeline(
    device: &ProtocolObject<dyn MTLDevice>,
    vert: &ProtocolObject<dyn MTLFunction>,
    frag: &ProtocolObject<dyn MTLFunction>,
    blending: bool,
) -> Result<Retained<ProtocolObject<dyn MTLRenderPipelineState>>, String> {
    let desc = MTLRenderPipelineDescriptor::new();
    desc.setVertexFunction(Some(vert));
    desc.setFragmentFunction(Some(frag));

    let ca = unsafe { desc.colorAttachments().objectAtIndexedSubscript(0) };
    ca.setPixelFormat(MTLPixelFormat::BGRA8Unorm);
    if blending {
        ca.setBlendingEnabled(true);
        // wl_shm defines alpha formats as premultiplied, so RGB must not be
        // multiplied by source alpha a second time during composition.
        ca.setSourceRGBBlendFactor(MTLBlendFactor::One);
        ca.setDestinationRGBBlendFactor(MTLBlendFactor::OneMinusSourceAlpha);
        ca.setSourceAlphaBlendFactor(MTLBlendFactor::One);
        ca.setDestinationAlphaBlendFactor(MTLBlendFactor::OneMinusSourceAlpha);
    }

    device
        .newRenderPipelineStateWithDescriptor_error(&desc)
        .map_err(|e: Retained<NSError>| format!("Pipeline error: {}", e.localizedDescription()))
}
