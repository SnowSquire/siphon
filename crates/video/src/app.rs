// ZGUI's component macro generates an associated method whose `self` is never read.
#![allow(clippy::unused_self)]

use std::path::PathBuf;
use std::sync::Arc;

use raw_window_handle::HasWindowHandle;
use zgui::platform::Surface;
use zgui::prelude::*;
use zgui::render::GpuUnavailable;
use zgui::render::RenderTarget;
use zgui::render::Renderer;
use zgui::runtime::AppError;
use zgui::surface::SurfaceConfig;
use zgui::surface::SurfaceHandle;
use zgui::surface::SurfaceIntrinsic;
use zgui::surface::wgpu;
use zgui_render_wgpu::Gpu;
use zgui_render_wgpu::SharedGraphics;

use crate::dxgi::DxgiSwapchain;
use crate::dxgi::hwnd_from_raw;
use crate::pattern;
use crate::renderer::DxgiRenderer;

pub fn run(file: Option<PathBuf>) -> anyhow::Result<()> {
    let file = file.map_or_else(
        || "no file selected".to_owned(),
        |path| path.display().to_string(),
    );

    let graphics = SharedGraphics::with_backends(wgpu::Backends::VULKAN);
    let gpu = graphics
        .open_gpu()
        .map_err(|error| anyhow::anyhow!("no Vulkan device: {error}"))?;

    let factory_gpu = Arc::clone(&gpu);

    app()
        .with_application_id("dev.siphon.video")
        .with_title("video")
        .with_size(1000.0, 660.0)
        .with_resizable(false)
        .with_stylesheet(SHEET)
        .with_renderer(Box::new(move |surface, target| {
            dxgi_renderer(&graphics, &factory_gpu, surface, target)
        }))
        .run(move || view! { Panel(file = file.clone()) })
        .map_err(|error| anyhow::anyhow!("zgui: {error}"))
}

fn dxgi_renderer(
    graphics: &SharedGraphics,
    gpu: &Arc<Gpu>,
    surface: &Arc<dyn Surface>,
    target: RenderTarget,
) -> Result<Box<dyn Renderer>, AppError> {
    let gpu_surface = Arc::clone(surface)
        .gpu_shared()
        .ok_or_else(|| dxgi_error("the surface has no native window handle"))?;
    let window = gpu_surface
        .window_handle()
        .map_err(|error| dxgi_error(format!("window handle: {error}")))?;
    let hwnd = hwnd_from_raw(window.as_raw()).map_err(|error| dxgi_error(format!("{error:#}")))?;
    let width = u32::try_from(target.size.width)
        .map_err(|_| dxgi_error("the target width is not positive"))?;
    let height = u32::try_from(target.size.height)
        .map_err(|_| dxgi_error("the target height is not positive"))?;

    let swapchain = DxgiSwapchain::new(gpu, hwnd, width, height)
        .map_err(|error| dxgi_error(format!("{error:#}")))?;
    let textures = swapchain.textures();
    let renderer = graphics
        .renderer_supplied(target, textures)
        .map_err(AppError::from)?;
    Ok(Box::new(DxgiRenderer::new(
        renderer,
        swapchain,
        Arc::clone(gpu),
    )))
}

fn dxgi_error(reason: impl Into<String>) -> AppError {
    AppError::GpuUnavailable(GpuUnavailable::new().rejected("DXGI", reason))
}

#[component]
fn Panel(file: String) -> impl IntoView {
    let video = SurfaceHandle::new(SurfaceConfig {
        premultiplied: true,
        intrinsic: SurfaceIntrinsic::ratio(16.0 / 9.0),
    });
    pattern::spawn(video.clone());

    let clicks = RwSignal::new(0u32);

    view! {
        column(class = "panel") {
            row(class = "header") {
                label(class = "title") { "video" }
            }
            {zgui::elements::surface()
                .class("preview")
                .source(&video)
                .into_view()}
            row(class = "footer") {
                label(class = "hint") {{move || file.clone()}}
                control(class = "button", on:click = move |_| clicks.update(|count| *count += 1)) {
                    "click"
                }
                label(class = "count") {{move || format!("clicks: {count}", count = clicks.get())}}
            }
        }
    }
}

const SHEET: &str = css!(
    ":root {
        background-color: #0e1016;
        color: #e8ecf4;
        font-family: sans-serif;
        display: flex;
        align-items: center;
        justify-content: center;
    }

    .panel {
        width: 900px;
        gap: 16px;
    }

    .header {
        align-items: baseline;
        gap: 10px;
    }

    .title {
        font-size: 24px;
        font-weight: 700;
    }

    .preview {
        width: 900px;
        height: 506px;
        border-radius: 12px;
        overflow: hidden;
        border: 1px solid #2c3342;
    }

    .footer {
        align-items: center;
        gap: 14px;
    }

    .hint {
        flex-grow: 1;
        color: #7d879b;
        font-size: 12px;
    }

    .button {
        padding: 7px 14px;
        border-radius: 7px;
        background-color: #2b6cff;
        color: #ffffff;
    }

    .button:hover {
        background-color: #3f7cff;
    }

    .count {
        color: #aab4c6;
        font-size: 12px;
    }"
);
