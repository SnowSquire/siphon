//! The renderer ZGUI draws through: the stock wgpu renderer, presenting into a DXGI swapchain.
//!
//! `renderer_supplied` writes each composed frame into one of the imported DXGI
//! backbuffers. After the copy has completed, `Present1` hands that backbuffer to
//! the compositor. Everything else is forwarded to the wgpu renderer, including
//! `as_any_mut` so the `surface` embed host can still reach it.

use std::any::Any;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use zgui::atlas::TextureSink;
use zgui::bits::DamageSet;
use zgui::prelude::GetUntracked as _;
use zgui::reactive::LocalStorage;
use zgui::reactive::RwSignal;
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
use zgui_render_wgpu::WgpuRenderer;

use crate::composite::Compositor;
use crate::dxgi::DxgiSwapchain;

pub struct DxgiRenderer {
    inner: WgpuRenderer,
    swapchain: Rc<RefCell<DxgiSwapchain>>,
    gpu: Arc<Gpu>,
    /// Kept alive for the compositor's bind group.
    _ui_view: wgpu::TextureView,
    compositor: Compositor,
    brightness: RwSignal<f64, LocalStorage>,
}

impl DxgiRenderer {
    pub fn new(
        inner: WgpuRenderer,
        swapchain: DxgiSwapchain,
        gpu: Arc<Gpu>,
        ui_view: wgpu::TextureView,
        compositor: Compositor,
        brightness: RwSignal<f64, LocalStorage>,
    ) -> Self {
        Self {
            inner,
            swapchain: Rc::new(RefCell::new(swapchain)),
            gpu,
            _ui_view: ui_view,
            compositor,
            brightness,
        }
    }

    /// Composites the UI into the current backbuffer and presents it.
    ///
    /// The copy into the UI texture and the composite are wgpu work on one
    /// queue, so the composite sees the frame's pixels. The wait before
    /// `Present1` is coarse and the first thing to replace with a fence.
    #[allow(clippy::cast_possible_truncation)]
    fn present(&self, outcome: FrameOutcome) -> FrameOutcome {
        if matches!(outcome, FrameOutcome::Presented(_) | FrameOutcome::Recovered) {
            let index = self.swapchain.borrow().current_index();
            let view = self.swapchain.borrow().view(index).cloned();
            if let Some(view) = view {
                let mut encoder = self
                    .gpu
                    .device()
                    .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                        label: Some("video.composite"),
                    });
                let nits = self.brightness.get_untracked() as f32;
                self.compositor
                    .render(self.gpu.queue(), &mut encoder, &view, nits);
                self.gpu.queue().submit([encoder.finish()]);
            }
            self.gpu.wait();
            if let Err(error) = self.swapchain.borrow().present() {
                eprintln!("DXGI present failed: {error:#}");
            }
        }
        outcome
    }
}

impl Renderer for DxgiRenderer {
    fn capabilities(&self) -> RenderCapabilities {
        self.inner.capabilities()
    }

    fn configure(&mut self, target: RenderTarget) {
        self.inner.configure(target);
    }

    fn target(&self) -> Option<RenderTarget> {
        self.inner.target()
    }

    fn draw(&mut self, scene: &Scene, damage: &DamageSet) -> FrameOutcome {
        // The inner renderer has one destination: the UI texture. The DXGI
        // backbuffer is chosen by the compositor's pass, not here.
        if !self.inner.present_into(0) {
            eprintln!("the UI texture was not supplied");
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
        let outcome = self.inner.present_held();
        self.present(outcome)
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
