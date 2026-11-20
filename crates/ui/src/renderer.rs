//! ZGUI's renderer, presenting through the platform's chain.
//!
//! ZGUI composes the interface into an ordinary `Bgra8Unorm` texture; the chain
//! draws that texture over the diagnostics fill and presents it. There is no
//! overlay renderer in between: the fill, the alpha compositing and the colour
//! conversion all happen in the chain's own shader.

use std::any::Any;
use std::sync::Arc;
use std::time::Duration;

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

pub struct UiRenderer {
    inner: WgpuRenderer,
    /// The platform's present chain: swapchain, pass and its policy.
    chain: Option<Chain>,
    /// ZGUI's interface, handed to the chain's pass when it is created.
    ui_texture: wgpu::Texture,
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
        ui_texture: wgpu::Texture,
        graphics: SharedGraphics,
        gpu: Arc<Gpu>,
        target: RenderTarget,
    ) -> Self {
        let (width, height) = (
            u32::try_from(target.size.width).unwrap_or(1),
            u32::try_from(target.size.height).unwrap_or(1),
        );
        Self {
            inner,
            chain: Some(chain),
            ui_texture,
            graphics,
            gpu,
            width,
            height,
        }
    }

    /// Paints the interface texture with the page background.
    ///
    /// ZGUI composes the regions it considers damaged, so a texture that was
    /// just created can be presented with rows nobody wrote yet. Those rows are
    /// transparent, which the fill would show as a band; starting from the page
    /// background makes a not-yet-painted row look like the interface instead.
    fn clear_ui_texture(&self, texture: &wgpu::Texture) {
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .gpu
            .device()
            .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        {
            let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("siphon.ui.background"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        // The page background, sRGB-encoded (#090E14).
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 9.0 / 255.0,
                            g: 14.0 / 255.0,
                            b: 18.0 / 255.0,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
        }
        self.gpu.queue().submit(Some(encoder.finish()));
    }

    /// Copies the interface's previous pixels into a freshly created texture.
    ///
    /// The overlap is 1:1 from the top-left, which is where the interface is
    /// anchored; pixels outside it stay at the background the clear left.
    fn carry_ui_texture(&self, texture: &wgpu::Texture) {
        let (old_width, old_height) = (self.width, self.height);
        let (new_width, new_height) = (texture.width(), texture.height());
        let width = old_width.min(new_width);
        let height = old_height.min(new_height);
        if width == 0 || height == 0 {
            return;
        }
        let mut encoder = self
            .gpu
            .device()
            .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        encoder.copy_texture_to_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.ui_texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
        self.gpu.queue().submit(Some(encoder.finish()));
    }

    /// Rebuilds the UI side of the presentation for a new target extent: the
    /// texture, the chain's binding to it and the inner renderer. The chain
    /// resizes itself in [`Chain::configure`].
    fn rebuild(&mut self, target: RenderTarget, width: u32, height: u32) {
        // Nothing is replaced until its replacement exists: a step below that
        // fails returns with the interface this renderer already had.
        let ui_texture = self
            .gpu
            .device()
            .create_texture(&wgpu::TextureDescriptor {
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
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                    | wgpu::TextureUsages::TEXTURE_BINDING
                    | wgpu::TextureUsages::COPY_SRC
                    | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
        // A supplied texture is copied into by damage only and otherwise keeps
        // what it holds, so a fresh one has to start as the frame that was on
        // screen, not as a blank: the rows ZGUI is not owed this frame would
        // otherwise show as bands.
        self.clear_ui_texture(&ui_texture);
        self.carry_ui_texture(&ui_texture);
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

        self.ui_texture = ui_texture;
        self.inner = inner;
        self.width = width;
        self.height = height;
        if let Some(chain) = self.chain.as_mut()
            && let Err(error) = chain.set_interface(&self.ui_texture)
        {
            tracing::warn!(target: "ui", "the interface descriptor could not be replaced: {error:?}");
        }
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

    fn present(&self, outcome: FrameOutcome) -> FrameOutcome {
        crate::chain::note_present();
        let Some(chain) = self.chain.as_ref() else {
            return outcome;
        };
        if !chain.presentable() {
            return outcome;
        }
        // Re-arm before anything can bail out: a held drag has no damage of its
        // own, so the next frame must not depend on this one presenting.
        chain.request_frame();
        if !chain.wait_for_frame(100) {
            return outcome;
        }
        if !chain.begin((self.width, self.height)) {
            return outcome;
        }
        if !chain.end(&self.ui_texture) {
            chain.present_failed();
        }
        outcome
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
        // `SIPHON_SYNC=handover`: the previous frame's pass must be finished
        // with the interface texture before ZGUI writes it again.
        if let Some(chain) = self.chain.as_ref() {
            chain.wait_handover();
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
