//! The presentation seam: the Windows chain, and the ZGUI plumbing above it.
//!
//! Everything above the chain — the ZGUI compose into a `Bgra8Unorm` texture and
//! the handover of that texture to the pass — is platform-free. The chain owns
//! the D3D12 swapchain, the Vulkan images behind its backbuffers, and the pass
//! that draws the interface over the diagnostics fill.
//!
//! There is no libplacebo here: the fill and the colour conversion are the
//! chain's own shaders, because the point of this build is to see whether the
//! banding survives without it.

use std::sync::Arc;

use zgui::platform::Surface;
use zgui::render::GpuUnavailable;
use zgui::render::RenderTarget;
use zgui::render::Renderer;
use zgui::runtime::AppError;
use zgui::surface::wgpu;
use zgui_render_wgpu::Gpu;
use zgui_render_wgpu::SharedGraphics;

use crate::renderer::UiRenderer;

#[cfg(windows)]
mod blit;
#[cfg(windows)]
mod pass;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::Chain;

/// How long the window's size has to hold still before a grown chain shrinks
/// back to it (Windows only).
///
/// Every resize event moves it, so a drag of any length pays one chain resize
/// when it starts and one this long after it stops. A pause in the middle of a
/// drag is indistinguishable from the end of one, and pays the same two.
#[cfg(windows)]
pub const SETTLE: std::time::Duration = std::time::Duration::from_millis(400);

/// The extent a chain holds once the window's size is settled: the window.
#[cfg(windows)]
#[must_use]
pub fn idle_canvas(width: u32, height: u32) -> (u32, u32) {
    (width.max(1), height.max(1))
}

/// The resize timer and extent of the Windows chain.
#[cfg(windows)]
pub struct Canvas {
    width: u32,
    height: u32,
    settle_at: Option<std::time::Instant>,
}

#[cfg(windows)]
impl Canvas {
    pub(crate) const fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            settle_at: None,
        }
    }

    pub(crate) const fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Whether the chain is at least as large as the window.
    pub(crate) const fn covers(&self, ui: (u32, u32)) -> bool {
        self.width >= ui.0 && self.height >= ui.1
    }

    /// Called whenever the window's extent changes.
    pub(crate) fn configured(&mut self) {
        self.settle_at = Some(std::time::Instant::now() + SETTLE);
    }

    pub(crate) const fn resized(&mut self, size: (u32, u32)) {
        self.width = size.0;
        self.height = size.1;
    }

    /// Whether the settle period has elapsed, consuming the timer.
    pub(crate) fn settled(&mut self) -> bool {
        let Some(at) = self.settle_at else {
            return false;
        };
        if std::time::Instant::now() < at {
            return false;
        }
        self.settle_at = None;
        true
    }
}

/// Builds the renderer for a window, wiring the shared parts to the platform's
/// chain. Called by `App::with_renderer`, once per window.
pub fn renderer(
    graphics: &SharedGraphics,
    gpu: &Arc<Gpu>,
    surface: &Arc<dyn Surface>,
    target: RenderTarget,
) -> Result<Box<dyn Renderer>, AppError> {
    let gpu_surface = Arc::clone(surface)
        .gpu_shared()
        .ok_or_else(|| gpu_error("the surface has no native window handle"))?;
    let width = u32::try_from(target.size.width)
        .map_err(|_| gpu_error("the target width is not positive"))?;
    let height = u32::try_from(target.size.height)
        .map_err(|_| gpu_error("the target height is not positive"))?;

    // The UI texture is always exactly the window's extent, whatever the chain
    // covers: ZGUI composes into it and the pass samples it over the rest of a
    // larger canvas, where its edge texels repeat.
    let ui = gpu.device().create_texture(&wgpu::TextureDescriptor {
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
        // COPY_SRC/COPY_DST: a rebuilt texture is seeded with the one it
        // replaces, because ZGUI only copies its damage into a supplied one.
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT
            | wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::COPY_SRC
            | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let mut chain = Chain::new(gpu, surface, &gpu_surface, width, height).map_err(|error| {
        eprintln!("chain: {error:?}");
        gpu_error(format!("{error}"))
    })?;
    chain
        .set_interface(&ui)
        .map_err(|error| gpu_error(format!("{error}")))?;
    let inner = graphics
        .renderer_supplied(target, vec![ui.clone()])
        .map_err(AppError::from)?;
    Ok(Box::new(UiRenderer::new(
        inner,
        chain,
        ui,
        graphics.clone(),
        Arc::clone(gpu),
        target,
    )))
}

/// Every failure here means the window cannot be presented by this backend.
pub fn gpu_error(reason: impl Into<String>) -> AppError {
    AppError::GpuUnavailable(GpuUnavailable::new().rejected("presentation", reason))
}

#[cfg(windows)]
pub use windows::note_present;

#[cfg(not(windows))]
pub const fn note_present() {}
