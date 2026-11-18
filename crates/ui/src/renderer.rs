//! ZGUI's renderer, presenting through the platform's chain.
//!
//! ZGUI composes the interface into an ordinary `Bgra8Unorm` texture and
//! libplacebo draws it into the frame the chain acquired. Which API presents
//! that frame — DXGI on Windows, a `VkSwapchainKHR` on Wayland — is the chain's
//! business, not this module's.

use std::any::Any;
use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use libplacebo_sys as pl;
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

use crate::chain::Chain;
use crate::placebo::Placebo;

pub struct UiRenderer {
    inner: WgpuRenderer,
    /// The platform's present chain: swapchain, its textures and its policy.
    chain: Option<Chain>,
    placebo: Rc<Placebo>,
    /// ZGUI's interface, wrapped for the overlay pass.
    ui: pl::pl_tex,
    /// Keeps the image behind `ui` alive; dropped after `ui` in `Drop`.
    ui_texture: wgpu::Texture,
    handover: pl::VkSemaphore,
    handover_value: Cell<u64>,
    graphics: SharedGraphics,
    gpu: Arc<Gpu>,
    /// The extent the UI texture and inner renderer are built for.
    width: u32,
    height: u32,
}

impl UiRenderer {
    #[allow(clippy::too_many_arguments, reason = "one renderer wiring call")]
    pub fn new(
        inner: WgpuRenderer,
        chain: Chain,
        placebo: Rc<Placebo>,
        ui_texture: wgpu::Texture,
        graphics: SharedGraphics,
        gpu: Arc<Gpu>,
        target: RenderTarget,
    ) -> eros::Result<Self> {
        let ui = placebo.wrap_wgpu(&ui_texture)?;
        let (width, height) = (
            u32::try_from(target.size.width).unwrap_or(1),
            u32::try_from(target.size.height).unwrap_or(1),
        );

        let handover = placebo.create_semaphore();
        eros::ensure!(
            !handover.is_null(),
            "libplacebo refused to create the handover semaphore"
        );

        Ok(Self {
            inner,
            chain: Some(chain),
            placebo,
            ui,
            ui_texture,
            handover,
            handover_value: Cell::new(0),
            graphics,
            gpu,
            width,
            height,
        })
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
                    max_luma: self.chain.as_ref().map_or(200.0, Chain::sdr_white),
                    max_cll: self.chain.as_ref().map_or(200.0, Chain::sdr_white),
                    ..Default::default()
                },
            },
            parts: parts.as_ptr(),
            num_parts: parts.len() as std::os::raw::c_int,
        }
    }

    /// Rebuilds the UI side of the presentation for a new target extent: the
    /// texture, its libplacebo view and the inner renderer. The chain resizes
    /// itself in [`Chain::configure`].
    fn rebuild(&mut self, target: RenderTarget, width: u32, height: u32) {
        // Nothing is replaced until its replacement exists: a step below that
        // fails returns with the interface this renderer already had, where
        // destroying it first would leave the frame that follows drawing
        // through a texture libplacebo has freed.
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
                tracing::warn!(target: "ui", "libplacebo: resized interface wrap failed: {error:?}");
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

        self.placebo.destroy_texture(&mut self.ui);
        self.ui = ui;
        self.ui_texture = ui_texture;
        self.inner = inner;
        self.width = width;
        self.height = height;
    }

    /// Applies a deferred configure: resize the chain, then rebuild the UI for
    /// the extent the chain actually granted.
    fn apply_configure(&mut self, target: RenderTarget) {
        let requested = (
            u32::try_from(target.size.width).unwrap_or(1),
            u32::try_from(target.size.height).unwrap_or(1),
        );
        let (width, height) = self
            .chain
            .as_mut()
            .map_or(requested, |chain| chain.configure(requested));
        self.rebuild(target, width, height);
    }

    fn present(&mut self, outcome: FrameOutcome) -> FrameOutcome {
        let Some(chain) = self.chain.as_ref() else {
            return outcome;
        };
        if !chain.presentable() {
            return outcome;
        }
        if !chain.wait_for_frame(100) {
            return outcome;
        }
        let Some(acquired) = chain.begin((self.width, self.height)) else {
            return outcome;
        };
        let (width, height) = (acquired.width, acquired.height);

        #[allow(
            clippy::cast_precision_loss,
            reason = "window sizes are well inside f32's exact integer range"
        )]
        let (ui_width_f, ui_height_f) = (self.width as f32, self.height as f32);
        let mut target = acquired.frame;

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

        // Declare the frame's own extent, not the surface's possibly stale one.
        let scale = self
            .inner
            .target()
            .map_or(1.0, |target| f64::from(target.scale.get()));
        if !chain.end(width, height, scale) {
            chain.present_failed();
        }
        outcome
    }
}

impl Drop for UiRenderer {
    fn drop(&mut self) {
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
        // KWin computes the final frame position from the geometry declared at
        // the start of a maximize. Declare the new extent now, before the
        // swapchain is rebuilt, so the window lands at the origin instead of
        // centred on its old size.
        if let Some(chain) = self.chain.as_ref() {
            chain.declare(width, height, target.scale.get());
        }
        self.apply_configure(target);
    }

    fn target(&self) -> Option<RenderTarget> {
        self.inner.target()
    }

    fn draw(&mut self, scene: &Scene, damage: &DamageSet) -> FrameOutcome {
        if let Some(chain) = self.chain.as_mut() {
            chain.settle((self.width, self.height));
        }
        if !self.inner.present_into(0) {
            tracing::warn!(target: "ui", "the UI texture was not supplied");
        }
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
