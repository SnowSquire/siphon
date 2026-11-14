//! ZGUI's renderer, presenting through the DXGI backbuffer.
//!
//! ZGUI composes the interface into an ordinary `Bgra8Unorm` texture, libplacebo
//! puts it into the current DXGI backbuffer, and DXGI presents. There is no
//! `VkSurfaceKHR` and no `VkSwapchainKHR`; the backbuffers are D3D12 resources
//! imported into Vulkan.

use std::any::Any;
use std::cell::Cell;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use libplacebo_sys as pl;
use windows::Win32::Foundation::HWND;
use zgui::atlas::TextureSink;
use zgui::bits::DamageSet;
use zgui::render::ExternalTexture;
use zgui::render::FrameOutcome;
use zgui::render::MemoryReport;
use zgui::render::RenderCapabilities;
use zgui::render::RenderTarget;
use zgui::render::Renderer;
use zgui::render::ScrollShift;
use zgui::render::TargetPoolReport;
use zgui::render::TextureHandle;
use zgui::render::VectorStatus;
use zgui::scene::Scene;
use zgui::surface::wgpu;
use zgui_render_wgpu::Gpu;
use zgui_render_wgpu::SharedGraphics;
use zgui_render_wgpu::WgpuRenderer;

use crate::dxgi::BUFFERS;
use crate::dxgi::D3d12;
use crate::dxgi::DxgiSwapchain;
use crate::dxgi::HdrMode;
use crate::dxgi::canvas_size;
use crate::placebo::Placebo;

pub struct UiRenderer {
    inner: WgpuRenderer,
    swapchain: RefCell<Option<DxgiSwapchain>>,
    placebo: Placebo,
    /// One libplacebo texture per backbuffer.
    targets: Vec<pl::pl_tex>,
    /// ZGUI's interface, wrapped for the overlay pass.
    ui: pl::pl_tex,
    /// Keeps the image behind `ui` alive; dropped after `ui` in `Drop`.
    ui_texture: wgpu::Texture,
    sdr_white: f32,
    handover: pl::VkSemaphore,
    handover_value: Cell<u64>,
    damaged: Cell<bool>,
    presented: Cell<bool>,
    graphics: SharedGraphics,
    gpu: Arc<Gpu>,
    d3d12: Rc<D3d12>,
    /// The extent the UI texture and inner renderer are built for.
    width: u32,
    height: u32,
    /// The extent of the chain's canvas, which is never smaller than the UI.
    chain_width: u32,
    chain_height: u32,
    hwnd: HWND,
}

impl UiRenderer {
    #[allow(clippy::too_many_arguments, reason = "one renderer wiring call")]
    pub fn new(
        inner: WgpuRenderer,
        swapchain: DxgiSwapchain,
        placebo: Placebo,
        ui_texture: wgpu::Texture,
        graphics: SharedGraphics,
        gpu: Arc<Gpu>,
        d3d12: Rc<D3d12>,
        hwnd: HWND,
        target: RenderTarget,
    ) -> anyhow::Result<Self> {
        let ui = placebo.wrap_wgpu(&ui_texture)?;
        let sdr_white = swapchain.sdr_white();
        let (width, height) = (
            u32::try_from(target.size.width).unwrap_or(1),
            u32::try_from(target.size.height).unwrap_or(1),
        );
        let (chain_width, chain_height) = swapchain.size();
        let mut targets = Vec::with_capacity(BUFFERS as usize);
        for index in 0..BUFFERS {
            let image = swapchain
                .image(index)
                .ok_or_else(|| anyhow::anyhow!("the swapchain has no backbuffer {index}"))?;
            let target = placebo.wrap(
                image,
                chain_width,
                chain_height,
                ash::vk::Format::A2B10G10R10_UNORM_PACK32,
                ash::vk::ImageUsageFlags::COLOR_ATTACHMENT
                    | ash::vk::ImageUsageFlags::TRANSFER_SRC
                    | ash::vk::ImageUsageFlags::TRANSFER_DST
                    | ash::vk::ImageUsageFlags::SAMPLED,
            )?;
            placebo.release(target, pl::VkImageLayout::VK_IMAGE_LAYOUT_UNDEFINED);
            targets.push(target);
        }

        let handover = placebo.create_semaphore();
        anyhow::ensure!(
            !handover.is_null(),
            "libplacebo refused to create the handover semaphore"
        );

        Ok(Self {
            inner,
            swapchain: RefCell::new(Some(swapchain)),
            placebo,
            targets,
            ui,
            ui_texture,
            sdr_white,
            handover,
            handover_value: Cell::new(0),
            damaged: Cell::new(true),
            presented: Cell::new(false),
            graphics,
            gpu,
            d3d12,
            width,
            height,
            chain_width,
            chain_height,
            hwnd,
        })
    }

    /// The backbuffer's SDR colour space.
    fn target_color(&self) -> pl::pl_color_space {
        pl::pl_color_space {
            primaries: pl::pl_color_primaries::PL_COLOR_PRIM_BT_709,
            transfer: pl::pl_color_transfer::PL_COLOR_TRC_BT_1886,
            hdr: pl::pl_hdr_metadata {
                max_luma: self.sdr_white,
                max_cll: self.sdr_white,
                ..Default::default()
            },
        }
    }

    /// The interface overlay, in ZGUI's sRGB space.
    fn ui_overlay(&self, parts: &[pl::pl_overlay_part]) -> pl::pl_overlay {
        pl::pl_overlay {
            tex: self.ui,
            mode: pl::pl_overlay_mode::PL_OVERLAY_NORMAL,
            coords: pl::pl_overlay_coords::PL_OVERLAY_COORDS_DST_FRAME,
            repr: pl::pl_color_repr {
                sys: pl::pl_color_system::PL_COLOR_SYSTEM_RGB,
                levels: pl::pl_color_levels::PL_COLOR_LEVELS_FULL,
                alpha: pl::pl_alpha_mode::PL_ALPHA_PREMULTIPLIED,
                ..Default::default()
            },
            color: pl::pl_color_space {
                primaries: pl::pl_color_primaries::PL_COLOR_PRIM_BT_709,
                transfer: pl::pl_color_transfer::PL_COLOR_TRC_SRGB,
                hdr: pl::pl_hdr_metadata {
                    max_luma: self.sdr_white,
                    max_cll: self.sdr_white,
                    ..Default::default()
                },
            },
            parts: parts.as_ptr(),
            num_parts: parts.len() as std::os::raw::c_int,
        }
    }

    /// Rebuilds the presentation for a new target extent: the chain's
    /// backbuffers when `chain` names a new size, the UI texture, its
    /// libplacebo view and the inner renderer.
    ///
    /// `chain` is `None` while the chain's canvas already covers the window;
    /// only the UI side is rebuilt then, which keeps the per-event cost at
    /// layout plus one texture and one renderer.
    fn rebuild(
        &mut self,
        target: RenderTarget,
        width: u32,
        height: u32,
        chain: Option<(u32, u32)>,
    ) {
        let started = std::time::Instant::now();
        self.placebo.destroy_texture(&mut self.ui);
        if let Some((chain_width, chain_height)) = chain
            && !self.resize_chain(chain_width, chain_height)
        {
            return;
        }

        let ui_texture = self.gpu.device().create_texture(&wgpu::TextureDescriptor {
            label: Some("siphon.ui"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Bgra8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let ui = match self.placebo.wrap_wgpu(&ui_texture) {
            Ok(ui) => ui,
            Err(error) => {
                tracing::warn!(target: "ui", "libplacebo: resized interface wrap failed: {error:#}");
                return;
            }
        };
        let mut inner = match self
            .graphics
            .renderer_supplied(target, vec![ui_texture.clone()])
        {
            Ok(inner) => inner,
            Err(error) => {
                tracing::warn!(target: "ui", "ZGUI: resize renderer failed: {error}");
                return;
            }
        };
        // The atlas *policy* lives above the renderer: the scene names its
        // glyph and image tiles by texture id and the window's content cache
        // believes those tiles are resident. A fresh renderer starts with
        // empty atlas textures, so without moving the old ones over every
        // piece of text would be sampled from a texture that no longer
        // exists. Rects are plain geometry and survive either way.
        std::mem::swap(self.inner.atlas(), inner.atlas());

        self.ui = ui;
        self.ui_texture = ui_texture;
        self.inner = inner;
        self.width = width;
        self.height = height;
        self.damaged.set(true);
        self.presented.set(false);
        tracing::debug!(
            target: "ui",
            width,
            height,
            elapsed = ?started.elapsed(),
            "rebuilt"
        );
    }

    /// Resizes the chain and re-wraps its backbuffers.
    fn resize_chain(&mut self, width: u32, height: u32) -> bool {
        for target in &mut self.targets {
            self.placebo.destroy_texture(target);
        }
        self.targets.clear();

        // The chain is reused through `ResizeBuffers`; creating a new one would
        // make the window association and the flip queue start over. A chain
        // that cannot be resized is dropped, and the next attempt creates one.
        let mut chain = self.swapchain.borrow_mut().take();
        let resized = match &mut chain {
            Some(existing) => existing.resize(&self.d3d12, &self.gpu, width, height),
            None => match DxgiSwapchain::new(
                &self.d3d12,
                &self.gpu,
                self.hwnd,
                width,
                height,
                HdrMode::Off,
            ) {
                Ok(fresh) => {
                    chain = Some(fresh);
                    Ok(())
                }
                Err(error) => Err(error),
            },
        };
        if let Err(error) = resized {
            tracing::warn!(target: "ui", "DXGI: resize failed: {error:#}");
            return false;
        }
        let Some(swapchain) = chain else {
            return false;
        };
        let mut targets = Vec::with_capacity(BUFFERS as usize);
        for index in 0..BUFFERS {
            let Some(image) = swapchain.image(index) else {
                return false;
            };
            match self.placebo.wrap(
                image,
                width,
                height,
                ash::vk::Format::A2B10G10R10_UNORM_PACK32,
                ash::vk::ImageUsageFlags::COLOR_ATTACHMENT
                    | ash::vk::ImageUsageFlags::TRANSFER_SRC
                    | ash::vk::ImageUsageFlags::TRANSFER_DST
                    | ash::vk::ImageUsageFlags::SAMPLED,
            ) {
                Ok(texture) => {
                    self.placebo
                        .release(texture, pl::VkImageLayout::VK_IMAGE_LAYOUT_UNDEFINED);
                    targets.push(texture);
                }
                Err(error) => {
                    tracing::warn!(target: "ui", "libplacebo: resize wrap failed: {error:#}");
                    return false;
                }
            }
        }
        self.targets = targets;
        self.chain_width = width;
        self.chain_height = height;
        *self.swapchain.borrow_mut() = Some(swapchain);
        true
    }

    fn present(&self, outcome: FrameOutcome) -> FrameOutcome {
        let swapchain = self.swapchain.borrow();
        let Some(swapchain) = swapchain.as_ref() else {
            return outcome;
        };
        let (width, height) = swapchain.size();
        let index = swapchain.current_index();
        let Some(&target_tex) = self.targets.get(usize::try_from(index).unwrap_or(0)) else {
            return outcome;
        };
        let damaged = self.damaged.replace(false);
        if !damaged && self.presented.get() {
            return outcome;
        }
        if !swapchain.wait_for_frame(100) {
            // Occluded or minimized: drop this frame instead of blocking.
            self.damaged.set(true);
            return outcome;
        }

        #[allow(
            clippy::cast_precision_loss,
            reason = "window sizes are well inside f32's exact integer range"
        )]
        let (ui_width_f, ui_height_f) = (self.width as f32, self.height as f32);
        let mut target = pl::pl_frame {
            num_planes: 1,
            planes: [pl::pl_plane {
                texture: target_tex,
                components: 3,
                component_mapping: [0, 1, 2, 0],
                ..Default::default()
            }; 4],
            repr: pl::pl_color_repr {
                sys: pl::pl_color_system::PL_COLOR_SYSTEM_RGB,
                levels: pl::pl_color_levels::PL_COLOR_LEVELS_FULL,
                alpha: pl::pl_alpha_mode::PL_ALPHA_NONE,
                ..Default::default()
            },
            color: self.target_color(),
            ..Default::default()
        };
        target.crop = pl::pl_rect2df {
            x0: 0.0,
            y0: 0.0,
            x1: width as f32,
            y1: height as f32,
        };

        // The canvas is larger than the UI whenever a drag is growing the
        // window. Extending the UI's own edge texels over the rest means the
        // area a frame exposes is the page background already, not black.
        let (width_f, height_f) = (width as f32, height as f32);
        let full = pl::pl_overlay_part {
            src: pl::pl_rect2df {
                x0: 0.0,
                y0: 0.0,
                x1: ui_width_f,
                y1: ui_height_f,
            },
            dst: pl::pl_rect2df {
                x0: 0.0,
                y0: 0.0,
                x1: ui_width_f,
                y1: ui_height_f,
            },
            color: [1.0, 1.0, 1.0, 1.0],
        };
        let mut parts = [full; 4];
        let mut count = 1;
        if width_f > ui_width_f {
            parts[count] = pl::pl_overlay_part {
                src: pl::pl_rect2df {
                    x0: ui_width_f - 1.0,
                    y0: 0.0,
                    x1: ui_width_f,
                    y1: ui_height_f,
                },
                dst: pl::pl_rect2df {
                    x0: ui_width_f,
                    y0: 0.0,
                    x1: width_f,
                    y1: ui_height_f,
                },
                color: [1.0, 1.0, 1.0, 1.0],
            };
            count += 1;
        }
        if height_f > ui_height_f {
            parts[count] = pl::pl_overlay_part {
                src: pl::pl_rect2df {
                    x0: 0.0,
                    y0: ui_height_f - 1.0,
                    x1: ui_width_f,
                    y1: ui_height_f,
                },
                dst: pl::pl_rect2df {
                    x0: 0.0,
                    y0: ui_height_f,
                    x1: ui_width_f,
                    y1: height_f,
                },
                color: [1.0, 1.0, 1.0, 1.0],
            };
            count += 1;
        }
        if width_f > ui_width_f && height_f > ui_height_f {
            parts[count] = pl::pl_overlay_part {
                src: pl::pl_rect2df {
                    x0: ui_width_f - 1.0,
                    y0: ui_height_f - 1.0,
                    x1: ui_width_f,
                    y1: ui_height_f,
                },
                dst: pl::pl_rect2df {
                    x0: ui_width_f,
                    y0: ui_height_f,
                    x1: width_f,
                    y1: height_f,
                },
                color: [1.0, 1.0, 1.0, 1.0],
            };
            count += 1;
        }
        let overlay = self.ui_overlay(&parts[..count]);
        target.overlays = &raw const overlay;
        target.num_overlays = 1;

        // ZGUI just finished a pass into the interface texture, so it is in
        // `COLOR_ATTACHMENT_OPTIMAL`; hand it to libplacebo and take it back.
        self.placebo.release(
            self.ui,
            pl::VkImageLayout::VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL,
        );
        if !self.placebo.render(None, &target) {
            tracing::warn!(target: "ui", "libplacebo: render failed for {width}x{height}");
        }
        self.placebo.finish();

        let value = self.handover_value.get() + 1;
        self.handover_value.set(value);
        let held = self.placebo.hold(
            self.ui,
            pl::VkImageLayout::VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL,
            pl::pl_vulkan_sem {
                sem: self.handover,
                value,
            },
        );
        if !held {
            tracing::warn!(target: "ui", "libplacebo: failed to take the interface texture back");
        }

        if let Err(error) = swapchain.present() {
            tracing::warn!(target: "ui", "DXGI present failed: {error:#}");
        }
        self.presented.set(true);
        outcome
    }
}

impl Drop for UiRenderer {
    fn drop(&mut self) {
        for target in &mut self.targets {
            self.placebo.destroy_texture(target);
        }
        self.placebo.destroy_texture(&mut self.ui);
        self.placebo.destroy_semaphore(&mut self.handover);
    }
}

impl Renderer for UiRenderer {
    fn capabilities(&self) -> RenderCapabilities {
        self.inner.capabilities()
    }

    fn configure(&mut self, target: RenderTarget) {
        let width = u32::try_from(target.size.width).unwrap_or(0);
        let height = u32::try_from(target.size.height).unwrap_or(0);
        if width == 0 || height == 0 {
            self.inner.configure(target);
            return;
        }
        if width == self.width && height == self.height {
            self.inner.configure(target);
            return;
        }
        // The UI follows the window exactly. The chain is a canvas that only
        // has to be at least as large as the window, so a drag never pays for
        // a backbuffer reallocation: the window shows the canvas's top-left
        // corner, cropped by the compositor.
        let chain = (self.chain_width < width || self.chain_height < height)
            .then(|| canvas_size(width, height));
        self.rebuild(target, width, height, chain);
    }

    fn target(&self) -> Option<RenderTarget> {
        self.inner.target()
    }

    fn draw(&mut self, scene: &Scene, damage: &DamageSet) -> FrameOutcome {
        if !self.inner.present_into(0) {
            tracing::warn!(target: "ui", "the UI texture was not supplied");
        }
        self.damaged.set(self.damaged.get() || !damage.is_empty());
        let outcome = self.inner.draw(scene, damage);
        self.present(outcome)
    }

    fn shifts_composed_pixels(&self) -> bool {
        self.inner.shifts_composed_pixels()
    }

    fn composed_survives_configure(&self) -> bool {
        self.inner.composed_survives_configure()
    }

    fn shift_composed(&mut self, shift: ScrollShift) {
        self.inner.shift_composed(shift);
    }

    fn register_external(&mut self, texture: ExternalTexture) -> TextureHandle {
        self.inner.register_external(texture)
    }

    fn release_external(&mut self, handle: TextureHandle) {
        self.inner.release_external(handle);
    }

    fn memory(&self) -> MemoryReport {
        self.inner.memory()
    }

    fn vector_status(&self) -> VectorStatus {
        self.inner.vector_status()
    }

    fn target_pool(&self) -> TargetPoolReport {
        self.inner.target_pool()
    }

    fn release_cached_targets(&mut self) -> u64 {
        self.inner.release_cached_targets()
    }

    fn release_idle_resources(&mut self) -> u64 {
        self.inner.release_idle_resources()
    }

    fn acquire_block(&self) -> Duration {
        self.inner.acquire_block()
    }

    fn present_held(&mut self) -> FrameOutcome {
        self.inner.present_held()
    }

    fn texture_sink(&mut self) -> &mut dyn TextureSink {
        self.inner.texture_sink()
    }

    fn as_any_mut(&mut self) -> Option<&mut dyn Any> {
        self.inner.as_any_mut()
    }

    fn as_any(&self) -> Option<&dyn Any> {
        self.inner.as_any()
    }
}
