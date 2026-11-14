// ZGUI's component macro generates an associated method whose `self` is never read.
#![allow(clippy::unused_self)]

use std::path::PathBuf;
use std::sync::Arc;

use raw_window_handle::HasWindowHandle;
use zgui::platform::Surface;
use zgui_ui::prelude::*;
use zgui::prelude::*;
use zgui::reactive::LocalStorage;
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

use crate::composite::Compositor;
use crate::dxgi::DxgiSwapchain;
use crate::dxgi::HdrMode;
use crate::dxgi::hwnd_from_raw;
use crate::pattern;
use crate::renderer::DxgiRenderer;
use crate::video;

pub fn run(file: Option<PathBuf>, hdr: HdrMode) -> anyhow::Result<()> {
    let file = file.map_or_else(String::new, |path| path.display().to_string());
    let test_pattern = file.is_empty();

    let (gpu, decode) = crate::gpu::create()?;
    match decode {
        Some(video) => eprintln!(
            "vulkan video: decoding HEVC on queue family {} (ops {:#x})",
            video.family, video.operations
        ),
        None => eprintln!("vulkan video: this device has no hardware HEVC decode"),
    }
    if !file.is_empty() {
        let video = decode
            .ok_or_else(|| anyhow::anyhow!("this device has no Vulkan video decode"))?;
        crate::hw::probe(&gpu, std::path::Path::new(&file), video)?;
    }
    let graphics = SharedGraphics::with_gpu(Arc::clone(&gpu));
    let placebo = crate::placebo::Placebo::import(&gpu)
        .map_err(|error| anyhow::anyhow!("libplacebo: {error:#}"))?;
    eprintln!(
        "libplacebo: imported (API {}, gpu {:?}, renderer {:?})",
        ffcine_libplacebo_sys::PL_API_VER,
        placebo.gpu(),
        placebo.renderer()
    );

    let factory_gpu = Arc::clone(&gpu);

    // The renderer factory runs before the first view is built, so the monitor's
    // reported peak luminance is already in `max_nits` by the time the slider
    // reads its range.
    let max_nits = RwSignal::new_local(1000.0f64);
    let brightness = RwSignal::new_local(203.0f64);

    app()
        .with_application_id("dev.siphon.video")
        .with_title("video")
        .with_size(1000.0, 760.0)
        .with_resizable(false)
        .with_stylesheet(SHEET)
        .with_renderer(Box::new(move |surface, target| {
            dxgi_renderer(
                &graphics,
                &factory_gpu,
                surface,
                target,
                hdr,
                max_nits,
                brightness,
                test_pattern,
            )
        }))
        .run(move || {
            view! {
                Panel(file = file.clone(), brightness = brightness, max_nits = max_nits)
            }
        })
        .map_err(|error| anyhow::anyhow!("zgui: {error}"))
}

fn dxgi_renderer(
    graphics: &SharedGraphics,
    gpu: &Arc<Gpu>,
    surface: &Arc<dyn Surface>,
    target: RenderTarget,
    hdr: HdrMode,
    max_nits: RwSignal<f64, LocalStorage>,
    brightness: RwSignal<f64, LocalStorage>,
    test_pattern: bool,
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

    let swapchain = DxgiSwapchain::new(gpu, hwnd, width, height, hdr)
        .map_err(|error| dxgi_error(format!("{error:#}")))?;

    let monitor_max = swapchain.max_luminance();
    eprintln!(
        "DXGI: {}x{}, {:?}, HDR {}, peak {monitor_max} nits",
        target.size.width,
        target.size.height,
        swapchain.format(),
        swapchain.hdr()
    );
    let max = if monitor_max > 0.0 {
        f64::from(monitor_max)
    } else {
        1000.0
    };
    max_nits.set(max);
    if brightness.get_untracked() > max {
        brightness.set(max);
    }

    let ui = gpu.device().create_texture(&wgpu::TextureDescriptor {
        label: Some("video.ui"),
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
    let ui_view = ui.create_view(&wgpu::TextureViewDescriptor::default());
    let compositor = Compositor::new(gpu.device(), &ui_view, swapchain.format(), swapchain.hdr());

    let renderer = graphics
        .renderer_supplied(target, vec![ui])
        .map_err(AppError::from)?;
    Ok(Box::new(DxgiRenderer::new(
        renderer,
        swapchain,
        Arc::clone(gpu),
        ui_view,
        compositor,
        brightness,
        test_pattern,
    )))
}

fn dxgi_error(reason: impl Into<String>) -> AppError {
    AppError::GpuUnavailable(GpuUnavailable::new().rejected("DXGI", reason))
}

#[component]
fn Panel(
    file: String,
    brightness: RwSignal<f64, LocalStorage>,
    max_nits: RwSignal<f64, LocalStorage>,
) -> impl IntoView {
    let source = SurfaceHandle::new(SurfaceConfig {
        premultiplied: true,
        intrinsic: SurfaceIntrinsic::ratio(16.0 / 9.0),
    });
    if file.is_empty() {
        pattern::spawn(source.clone());
    } else {
        video::spawn(source.clone(), PathBuf::from(&file));
    }

    eprintln!("UI: brightness slider max {} nits", max_nits.get_untracked());
    let clicks = RwSignal::new(0u32);

    view! {
        column(class = "panel") {
            row(class = "header") {
                label(class = "title") { "video" }
            }
            {zgui::elements::surface()
                .class("preview")
                .source(&source)
                .into_view()}
            row(class = "brightness") {
                label(class = "hint") { "HDR test" }
                Slider(
                    value = brightness,
                    min = 0.0,
                    max = max_nits.get_untracked(),
                    step = 10.0,
                    label = "Brightness (nits)".to_owned(),
                    class = "slider",
                )
                label(class = "count") {{move || format!("{:.0} / {:.0} nits", brightness.get(), max_nits.get())}}
            }
            row(class = "footer") {
                label(class = "hint") {{move || if file.is_empty() { "no file selected".to_owned() } else { file.clone() }}}
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
        height: 375px;
        border-radius: 12px;
        overflow: hidden;
        border: 1px solid #2c3342;
    }

    .brightness {
        align-items: center;
        gap: 14px;
    }

    .slider {
        width: 380px;
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
