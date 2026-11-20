// ZGUI's component macro generates an associated method whose `self` is never read.
#![allow(clippy::unused_self)]

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use raw_window_handle::HasWindowHandle;
use zgui::platform::Surface;
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

use crate::dxgi::DxgiSwapchain;
use crate::dxgi::HdrMode;
use crate::dxgi::hwnd_from_raw;
use crate::gpu::VideoDecode;
use crate::placebo::Placebo;
use crate::player::Player;
use crate::renderer::DxgiRenderer;
use crate::video;

pub fn run(file: Option<PathBuf>, hdr: HdrMode) -> anyhow::Result<()> {
    let file = file.map_or_else(String::new, |path| path.display().to_string());
    let fps = if file.is_empty() {
        30.0
    } else {
        video::frame_rate(Path::new(&file))
    };

    let (gpu, decode) = crate::gpu::create()?;
    match decode {
        Some(video) => eprintln!(
            "vulkan video: decoding HEVC on queue family {} (ops {:#x})",
            video.family, video.operations
        ),
        None => eprintln!("vulkan video: this device has no hardware HEVC decode"),
    }
    if !file.is_empty() {
        let video =
            decode.ok_or_else(|| anyhow::anyhow!("this device has no Vulkan video decode"))?;
        crate::hw::probe(&gpu, Path::new(&file), video)?;
    }
    let graphics = SharedGraphics::with_gpu(Arc::clone(&gpu));
    let factory_gpu = Arc::clone(&gpu);
    let factory_file = PathBuf::from(&file);

    // What the display turned out to be. The renderer factory runs before the
    // first view is built, so the values are already set when it reads them.
    let peak_nits = RwSignal::new_local(0.0f64);
    let sdr_white = RwSignal::new_local(0.0f64);
    let hdr_enabled = RwSignal::new_local(false);

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
                decode,
                &factory_file,
                peak_nits,
                sdr_white,
                hdr_enabled,
            )
        }))
        .run(move || {
            view! {
                Panel(
                    file = file.clone(),
                    peak_nits = peak_nits,
                    sdr_white = sdr_white,
                    hdr_enabled = hdr_enabled,
                    fps = fps,
                )
            }
        })
        .map_err(|error| anyhow::anyhow!("zgui: {error}"))
}

#[allow(
    clippy::too_many_arguments,
    reason = "one factory call: the app's state is all separate signals"
)]
fn dxgi_renderer(
    graphics: &SharedGraphics,
    gpu: &Arc<Gpu>,
    surface: &Arc<dyn Surface>,
    target: RenderTarget,
    hdr: HdrMode,
    decode: Option<VideoDecode>,
    file: &Path,
    peak_nits: RwSignal<f64, LocalStorage>,
    sdr_white: RwSignal<f64, LocalStorage>,
    hdr_enabled: RwSignal<bool, LocalStorage>,
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
    peak_nits.set(f64::from(reported_peak(monitor_max)));
    sdr_white.set(f64::from(swapchain.sdr_white()));
    hdr_enabled.set(swapchain.hdr());

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

    let placebo =
        Placebo::import(gpu).map_err(|error| dxgi_error(format!("libplacebo: {error:#}")))?;
    eprintln!(
        "libplacebo: imported (API {}, renderer {:?})",
        libplacebo_sys::PL_API_VER,
        placebo.renderer()
    );

    let player = if file.as_os_str().is_empty() {
        None
    } else {
        let video = decode.ok_or_else(|| dxgi_error("this device has no Vulkan video decode"))?;
        Some(Player::open(gpu, video, file).map_err(|error| dxgi_error(format!("{error:#}")))?)
    };

    let renderer = graphics
        .renderer_supplied(target, vec![ui.clone()])
        .map_err(AppError::from)?;
    let renderer = DxgiRenderer::new(renderer, swapchain, placebo, ui, player)
        .map_err(|error| dxgi_error(format!("{error:#}")))?;
    Ok(Box::new(renderer))
}

fn dxgi_error(reason: impl Into<String>) -> AppError {
    AppError::GpuUnavailable(GpuUnavailable::new().rejected("DXGI", reason))
}

/// What to show for a peak luminance the display did not report.
fn reported_peak(reported: f32) -> f32 {
    if reported > 0.0 { reported } else { 1000.0 }
}

#[component]
fn Panel(
    file: String,
    peak_nits: RwSignal<f64, LocalStorage>,
    sdr_white: RwSignal<f64, LocalStorage>,
    hdr_enabled: RwSignal<bool, LocalStorage>,
    fps: f64,
) -> impl IntoView {
    let source = SurfaceHandle::new(SurfaceConfig {
        premultiplied: true,
        intrinsic: SurfaceIntrinsic::ratio(16.0 / 9.0),
    });
    // The pump runs for any file; with none, the interface is all there is, and
    // the window still has to be told when to draw.
    video::spawn(source.clone(), fps);

    view! {
        column(class = "panel") {
            row(class = "header") {
                label(class = "title") { "video" }
            }
            {zgui::elements::surface()
                .class("pump")
                .source(&source)
                .into_view()}
            row(class = "display") {
                label(class = "hint") {{move || {
                    format!(
                        "{} · peak {:.0} nits · SDR white {:.0} nits",
                        if hdr_enabled.get() { "HDR10 (PQ / Rec.2020)" } else { "SDR" },
                        peak_nits.get(),
                        sdr_white.get(),
                    )
                }}}
                label(class = "count") {{move || format!("{fps:.3} fps")}}
            }
            row(class = "footer") {
                label(class = "hint") {{move || if file.is_empty() { "no file selected".to_owned() } else { file.clone() }}}
            }
        }
    }
}

const SHEET: &str = css!(
    ":root {
        background-color: transparent;
        color: #e8ecf4;
        font-family: sans-serif;
        display: flex;
        align-items: flex-end;
        justify-content: center;
        padding-bottom: 24px;
    }

    .panel {
        width: 900px;
        gap: 16px;
        padding: 18px 22px;
        border-radius: 14px;
        background-color: rgba(14, 16, 22, 0.6);
    }

    .header {
        align-items: baseline;
        gap: 10px;
    }

    .title {
        font-size: 24px;
        font-weight: 700;
    }

    /* The frame pump. It asks for a frame at the video's rate and paints
       nothing, so it takes no space in the layout. */
    .pump {
        width: 1px;
        height: 1px;
    }

    .display {
        align-items: center;
        gap: 14px;
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
