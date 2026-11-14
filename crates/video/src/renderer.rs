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
use zgui_render_wgpu::Gpu;
use zgui_render_wgpu::WgpuRenderer;

use crate::dxgi::DxgiSwapchain;

pub struct DxgiRenderer {
    inner: WgpuRenderer,
    swapchain: Rc<RefCell<DxgiSwapchain>>,
    gpu: Arc<Gpu>,
}

impl DxgiRenderer {
    pub fn new(inner: WgpuRenderer, swapchain: DxgiSwapchain, gpu: Arc<Gpu>) -> Self {
        Self {
            inner,
            swapchain: Rc::new(RefCell::new(swapchain)),
            gpu,
        }
    }

    /// Presents a frame that reached the supplied backbuffer.
    ///
    /// The copy into the imported image is wgpu work, so wait for it before DXGI
    /// hands the buffer to the compositor. This is a coarse wait and the first
    /// thing to replace with a fence or semaphore once timing matters.
    fn present(&self, outcome: FrameOutcome) -> FrameOutcome {
        if matches!(outcome, FrameOutcome::Presented(_) | FrameOutcome::Recovered) {
            self.gpu.wait();
            if let Err(error) = self.swapchain.borrow_mut().present() {
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
        let index = self.swapchain.borrow().current_index() as usize;
        if !self.inner.present_into(index) {
            eprintln!("DXGI backbuffer index {index} is outside the supplied textures");
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
