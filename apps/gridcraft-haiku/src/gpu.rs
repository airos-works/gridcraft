//! Hardware rendering of the window, as the other Craft hosts on air/OS do it. Haiku has no wgpu
//! surface: egui's triangles draw with egui-wgpu into the presenter's frame texture
//! (`present.rs`), which gets them onto the screen. The software rasteriser (`raster.rs`) stays
//! for machines without a usable hardware Vulkan adapter, `GRIDCRAFT_RENDERER=cpu`, and a device
//! that is lost or fails while running.
//!
//! The device is asked for wgpu's downlevel limits (what every Vulkan driver of the air/OS images
//! offers, V3DV on the Pi 4 included) with the adapter's own largest texture size: egui needs no
//! optional feature, and the presenter's tile comparison needs only a small compute pass.
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use egui_wgpu::{Renderer, RendererOptions, ScreenDescriptor};

use crate::present::{self, Presenter, Shown};

/// `GRIDCRAFT_PRESENT`: how frames reach the window (`present::Mode`).
const PRESENT_VAR: &str = "GRIDCRAFT_PRESENT";
const LOG: &str = "GridCraft::gpu";
/// The smallest largest-texture an adapter must offer: a window this wide at least, and egui's
/// font atlas.
const MIN_TEXTURE_SIDE: u32 = 4096;

pub struct Gpu {
    device: wgpu::Device,
    queue: wgpu::Queue,
    renderer: Renderer,
    presenter: Presenter,
    /// Set by the device's lost and uncaptured-error callbacks (any thread): the window goes back
    /// to the software rasteriser.
    fault: Arc<AtomicBool>,
    /// What the frame on the window was drawn from ([`frame_key`], and the texture uploads so
    /// far): a frame drawn from the same is not drawn again. egui repaints more often than its
    /// output changes (a hover settling, a reply waiting on the next frame), and the software
    /// rasteriser skips those frames for nothing; here each would cost a submission and two
    /// waits for the GPU.
    last: Option<(u64, u64)>,
    /// Bumped by every texture upload, which can change the frame's pixels.
    textures: u64,
}

/// The limits the device is created with on `adapter`: wgpu's downlevel ones, with the adapter's
/// largest texture side (egui's atlas and the window-sized frame textures) and buffer size (the
/// frame's readback), within what the window can need.
fn required_limits(adapter: &wgpu::Limits) -> wgpu::Limits {
    let base = wgpu::Limits::downlevel_defaults();
    wgpu::Limits {
        max_texture_dimension_2d: adapter.max_texture_dimension_2d.clamp(base.max_texture_dimension_2d, 16384),
        max_buffer_size: adapter.max_buffer_size.clamp(base.max_buffer_size, 512 << 20),
        ..base
    }
}

/// Why `adapter` can't draw the window, or `None` when it can.
fn unusable(adapter: &wgpu::Adapter) -> Option<String> {
    let info = adapter.get_info();
    if !matches!(info.device_type, wgpu::DeviceType::DiscreteGpu | wgpu::DeviceType::IntegratedGpu) {
        // Software Vulkan (lavapipe) draws slower than the rasteriser does.
        return Some(format!("{:?} device, not a hardware GPU", info.device_type));
    }
    let limits = adapter.limits();
    if limits.max_texture_dimension_2d < MIN_TEXTURE_SIDE {
        return Some(format!("largest texture {} px, needs {MIN_TEXTURE_SIDE}", limits.max_texture_dimension_2d));
    }
    let mut short = Vec::new();
    required_limits(&limits).check_limits_with_fail_fn(&limits, false, |name, want, has| short.push(format!("{name} {has} < {want}")));
    if !short.is_empty() {
        return Some(format!("limits too small: {}", short.join(", ")));
    }
    let downlevel = adapter.get_downlevel_capabilities();
    if !downlevel.flags.contains(wgpu::DownlevelFlags::COMPUTE_SHADERS) {
        return Some("no compute shaders".into());
    }
    let formats = adapter.get_texture_format_features(present::FORMAT);
    let usage = wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_SRC | wgpu::TextureUsages::COPY_DST;
    if !formats.allowed_usages.contains(usage) {
        return Some(format!("{:?} frames not supported ({:?})", present::FORMAT, formats.allowed_usages));
    }
    None
}

/// The hardware adapter to draw with: a discrete GPU before an integrated one. Every adapter
/// that is passed over is logged with the reason.
fn choose_adapter(adapters: &[wgpu::Adapter]) -> Result<wgpu::Adapter, String> {
    if adapters.is_empty() {
        return Err("no Vulkan adapter (no Vulkan driver for this GPU, or no libvulkan)".into());
    }
    let mut best: Option<&wgpu::Adapter> = None;
    for adapter in adapters {
        let info = adapter.get_info();
        match unusable(adapter) {
            Some(why) => log::info!(target: LOG, "Vulkan adapter {} ({}, {}): not used: {why}", info.name, info.driver, info.driver_info),
            None => {
                log::info!(target: LOG, "Vulkan adapter {} ({}, {}): usable, {:?}", info.name, info.driver, info.driver_info, info.device_type);
                let discrete = |a: &wgpu::Adapter| a.get_info().device_type == wgpu::DeviceType::DiscreteGpu;
                if best.is_none_or(|b| !discrete(b) && discrete(adapter)) {
                    best = Some(adapter);
                }
            }
        }
    }
    best.cloned().ok_or_else(|| "no usable hardware Vulkan adapter".into())
}

impl Gpu {
    /// The device, its egui renderer and the window's presenter; an error (logged by the caller)
    /// leaves the window to the rasteriser.
    pub fn new() -> Result<Self, String> {
        let start = Instant::now();
        let mut desc = wgpu::InstanceDescriptor::new_without_display_handle_from_env();
        desc.backends = wgpu::Backends::VULKAN;
        let instance = wgpu::Instance::new(desc);
        let adapters = pollster::block_on(instance.enumerate_adapters(wgpu::Backends::VULKAN));
        let adapter = choose_adapter(&adapters)?;
        drop(adapters);
        let device_desc = wgpu::DeviceDescriptor {
            label: Some("GridCraft"),
            required_features: wgpu::Features::empty(),
            required_limits: required_limits(&adapter.limits()),
            ..Default::default()
        };
        let (device, queue) = pollster::block_on(adapter.request_device(&device_desc)).map_err(|e| format!("GPU device: {e}"))?;
        let fault = Arc::new(AtomicBool::new(false));
        let lost = fault.clone();
        device.set_device_lost_callback(move |reason, message| {
            log::error!(target: LOG, "GPU device lost ({reason:?}): {message}");
            lost.store(true, Ordering::Relaxed);
        });
        // wgpu's default handler panics.
        let failed = fault.clone();
        device.on_uncaptured_error(Arc::new(move |e| {
            log::error!(target: LOG, "GPU error: {e}");
            failed.store(true, Ordering::Relaxed);
        }));
        // Frames are drawn in the screen's byte order: they go to the frame buffer, or into a
        // B_RGB32 bitmap, as they are.
        let renderer = Renderer::new(&device, present::FORMAT, RendererOptions::default());
        let presenter = Presenter::new(&device, &queue, present::Mode::from_env(PRESENT_VAR), &crate::host::Native);
        // Pipelines that failed validation reported it to the handler above.
        if fault.load(Ordering::Relaxed) {
            return Err("the GPU couldn't build the renderer's pipelines".into());
        }
        let info = adapter.get_info();
        log::info!(
            target: LOG,
            "air/OS GPU: {} ({} {}), largest texture {} px; present: {}; ready in {:.0} ms",
            info.name,
            info.driver,
            info.driver_info,
            device.limits().max_texture_dimension_2d,
            presenter.mode().name(),
            start.elapsed().as_secs_f64() * 1e3
        );
        Ok(Self { device, queue, renderer, presenter, fault, last: None, textures: 0 })
    }

    /// egui's texture uploads, before the frames that use them.
    pub fn set_textures(&mut self, delta: &egui::TexturesDelta) {
        if delta.set.is_empty() {
            return;
        }
        self.textures += 1;
        for (id, images) in &delta.set {
            for image in images {
                self.renderer.update_texture(&self.device, &self.queue, *id, image);
            }
        }
    }

    /// Textures egui freed, after the frame that last used them.
    pub fn free_textures(&mut self, delta: &egui::TexturesDelta) {
        for id in &delta.free {
            self.renderer.free_texture(id);
        }
    }

    pub fn max_texture_side(&self) -> usize {
        self.device.limits().max_texture_dimension_2d as usize
    }

    /// Whether the device was lost or failed: the window should go back to the rasteriser.
    pub fn faulted(&self) -> bool {
        self.fault.load(Ordering::Relaxed)
    }

    /// Draws the frame and shows it (`scale` device pixels per logical pixel). The textures it
    /// uses are uploaded ([`Gpu::set_textures`]).
    pub fn render(&mut self, size: [u32; 2], primitives: &[egui::ClippedPrimitive], ppp: f32, clear: egui::Color32, scale: f32) -> Result<Shown, String> {
        let key = frame_key(size, primitives, ppp, clear).map(|k| (k, self.textures));
        if key.is_some() && key == self.last && !self.presenter.wants_present() {
            return Ok(Shown::default());
        }
        self.last = None;
        let view = self.presenter.target(size)?.clone();
        let start = Instant::now();
        let screen = ScreenDescriptor { size_in_pixels: size, pixels_per_point: ppp };
        let mut encoder = self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("GridCraft frame") });
        let mut commands = self.renderer.update_buffers(&self.device, &self.queue, &mut encoder, primitives, &screen);
        {
            let rgba = clear.to_array().map(|c| f64::from(c) / 255.0);
            let mut pass = encoder
                .begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("GridCraft UI"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &view,
                        resolve_target: None,
                        depth_slice: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color { r: rgba[0], g: rgba[1], b: rgba[2], a: rgba[3] }),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                })
                .forget_lifetime();
            self.renderer.render(&mut pass, primitives, &screen);
        }
        commands.push(encoder.finish());
        let encoded = start.elapsed();
        let shown = self.presenter.present(commands, &crate::host::Native, scale)?;
        if self.faulted() {
            return Err("the GPU failed drawing the frame".into());
        }
        self.last = key;
        log::debug!(target: LOG,
            "GPU frame {}x{}: encode {:.2} ms, present {:.2} ms ({}, {} px in {} rects)",
            size[0],
            size[1],
            encoded.as_secs_f64() * 1e3,
            shown.wait.as_secs_f64() * 1e3,
            if shown.direct { "direct" } else { "readback" },
            shown.pixels,
            shown.rects
        );
        Ok(shown)
    }

    /// The last frame again, where the window's place on the screen changed.
    pub fn present_again(&mut self, scale: f32) -> Result<Shown, String> {
        self.presenter.present(Vec::new(), &crate::host::Native, scale)
    }

    pub fn wants_present(&self) -> bool {
        self.presenter.wants_present()
    }

    pub fn sync_due(&self) -> Option<Instant> {
        self.presenter.sync_due()
    }

    /// Brings app_server's copy of the window up to date with what went straight to the screen.
    pub fn sync(&mut self, scale: f32) -> Result<Shown, String> {
        self.presenter.sync(&crate::host::Native, scale)
    }

    pub fn image(&mut self) -> Result<egui::ColorImage, String> {
        self.presenter.image()
    }
}

impl Drop for Gpu {
    fn drop(&mut self) {
        self.presenter.detach(&crate::host::Native);
        // Unwinding from a GPU fault, nothing more is asked of the device.
        if !std::thread::panicking() && !self.faulted() {
            let _ = self.device.poll(wgpu::PollType::Wait { submission_index: None, timeout: Some(Duration::from_secs(2)) });
        }
    }
}

/// A small, fast hash (FxHash's mix, as `raster.rs`) of everything that decides a frame's pixels,
/// or `None` for a frame that must be drawn (a paint callback draws what the hash can't see).
/// Texture uploads are checked by the caller.
fn frame_key(size: [u32; 2], primitives: &[egui::ClippedPrimitive], ppp: f32, clear: egui::Color32) -> Option<u64> {
    let mut k: u64 = 0;
    let mut add = |v: u64| k = (k.rotate_left(5) ^ v).wrapping_mul(0x51_7c_c1_b7_27_22_0a_95);
    add(u64::from(size[0]) << 32 | u64::from(size[1]));
    add(u64::from(ppp.to_bits()));
    add(u64::from(u32::from_le_bytes(clear.to_array())));
    for p in primitives {
        let r = p.clip_rect;
        add(u64::from(r.min.x.to_bits()) << 32 | u64::from(r.min.y.to_bits()));
        add(u64::from(r.max.x.to_bits()) << 32 | u64::from(r.max.y.to_bits()));
        let egui::epaint::Primitive::Mesh(mesh) = &p.primitive else { return None };
        add(match mesh.texture_id {
            egui::TextureId::Managed(n) => n,
            egui::TextureId::User(n) => n ^ (1 << 63),
        });
        add(mesh.indices.len() as u64);
        for &i in &mesh.indices {
            add(u64::from(i));
        }
        for v in &mesh.vertices {
            add(u64::from(v.pos.x.to_bits()) << 32 | u64::from(v.pos.y.to_bits()));
            add(u64::from(v.uv.x.to_bits()) << 32 | u64::from(v.uv.y.to_bits()));
            add(u64::from(u32::from_le_bytes(v.color.to_array())));
        }
    }
    Some(k)
}
