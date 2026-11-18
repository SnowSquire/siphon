//! The Windows chain: a D3D12 flip chain whose backbuffers are Vulkan images.
//!
//! The ZGUI interface is composed into a wgpu texture and libplacebo draws it
//! into the current backbuffer. The per-buffer `pl_tex` cache, the device-lost
//! latch and the monitor-sized resize canvas all live here; nothing above the
//! chain knows DXGI exists.

use std::rc::Rc;
use std::sync::Arc;

use eros::Context as _;
use libplacebo_sys as pl;
use raw_window_handle::HasWindowHandle as _;
use tracing::warn;
use windows::Win32::Foundation::HWND;
use zgui::platform::GpuSurface;
use zgui::platform::Surface;
use zgui_render_wgpu::Gpu;

use super::Acquired;
use super::Canvas;
use super::idle_canvas;
use crate::dxgi::BUFFERS;
use crate::dxgi::D3d12;
use crate::dxgi::DxgiSwapchain;
use crate::dxgi::HdrMode;
use crate::dxgi::hwnd_from_raw;
use crate::dxgi::resize_canvas;
use crate::dxgi::set_window_background;
use crate::placebo::Placebo;

/// The D3D12 device, the swapchain and the Vulkan views of its backbuffers.
pub struct Chain {
    d3d12: Rc<D3d12>,
    gpu: Arc<Gpu>,
    placebo: Rc<Placebo>,
    hwnd: HWND,
    swapchain: Option<DxgiSwapchain>,
    /// One libplacebo texture per backbuffer.
    targets: Vec<pl::pl_tex>,
    canvas: Canvas,
    sdr_white: f32,
}

impl Chain {
    pub fn new(
        gpu: &Arc<Gpu>,
        placebo: Rc<Placebo>,
        _surface: &Arc<dyn Surface>,
        gpu_surface: &Arc<dyn GpuSurface>,
        width: u32,
        height: u32,
    ) -> eros::Result<Self> {
        let window = gpu_surface
            .window_handle()
            .map_err(|error| eros::error!("window handle: {error}"))?;
        let hwnd = hwnd_from_raw(window.as_raw())?;
        set_window_background(hwnd);
        // The icon helper speaks `windows-sys`, whose `HWND` is the raw handle;
        // `hwnd` is the `windows` crate's newtype around the same pointer.
        crate::tray::set_window_icon(hwnd.0);

        let d3d12 = Rc::new(D3d12::new(gpu).context("D3D12")?);
        let mut this = Self {
            d3d12,
            gpu: Arc::clone(gpu),
            placebo,
            hwnd,
            swapchain: None,
            targets: Vec::new(),
            canvas: Canvas::new(width, height),
            sdr_white: 200.0,
        };
        this.rebuild(width, height)?;
        Ok(this)
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

    /// Resizes the chain and wraps its new backbuffers, or creates it the first
    /// time. The caller has destroyed every libplacebo texture over the old
    /// images; the D3D12 resources they named go when `ResizeBuffers` runs.
    fn rebuild(&mut self, width: u32, height: u32) -> eros::Result<()> {
        let placebo = Rc::clone(&self.placebo);
        for mut target in self.targets.drain(..) {
            placebo.destroy_texture(&mut target);
        }
        // The views are gone, but the frames that used them may not be: the
        // images are freed next, and a driver handed a free while the GPU still
        // names the image is free to do anything.
        placebo.finish();

        // The chain is reused through `ResizeBuffers`; creating a new one would
        // make the window association and the flip queue start over. A chain
        // that cannot be resized is dropped, and the next attempt creates one.
        let resized = match self.swapchain.as_mut() {
            Some(existing) => existing.resize(&self.d3d12, &self.gpu, width, height),
            None => DxgiSwapchain::new(
                &self.d3d12,
                &self.gpu,
                self.hwnd,
                width,
                height,
                HdrMode::Off,
            )
            .map(|fresh| {
                self.swapchain = Some(fresh);
            }),
        };
        if let Err(error) = resized {
            warn!(target: "ui", "DXGI: resize failed: {error:?}");
            return Err(error);
        }
        let Some(swapchain) = self.swapchain.as_ref() else {
            return Err(eros::error!("the swapchain was not created"));
        };
        let mut targets = Vec::with_capacity(BUFFERS as usize);
        for index in 0..BUFFERS {
            let image = swapchain
                .image(index)
                .ok_or_else(|| eros::error!("the swapchain has no backbuffer {index}"))?;
            let target = placebo.wrap(
                image,
                width,
                height,
                ash::vk::Format::A2B10G10R10_UNORM_PACK32,
                ash::vk::ImageUsageFlags::COLOR_ATTACHMENT
                    | ash::vk::ImageUsageFlags::TRANSFER_SRC
                    | ash::vk::ImageUsageFlags::TRANSFER_DST
                    | ash::vk::ImageUsageFlags::SAMPLED,
            )?;
            placebo.release(target, pl::VkImageLayout::VK_IMAGE_LAYOUT_UNDEFINED);
            targets.push(target);
        }
        self.sdr_white = swapchain.sdr_white();
        self.targets = targets;
        self.canvas.resized((width, height));
        Ok(())
    }

    /// Whether the driver has taken the device away.
    pub fn presentable(&self) -> bool {
        self.swapchain
            .as_ref()
            .is_none_or(DxgiSwapchain::is_removed)
    }

    pub const fn sdr_white(&self) -> f32 {
        self.sdr_white
    }

    /// Waits until the flip queue has room for another frame.
    pub fn wait_for_frame(&self, timeout_ms: u32) -> bool {
        self.swapchain
            .as_ref()
            .is_some_and(|chain| chain.wait_for_frame(timeout_ms))
    }

    /// Called when the window's extent changes. The UI follows it exactly; the
    /// chain grows to the monitor only while the window outgrows its canvas,
    /// and shrinks back in [`Chain::settle`].
    pub fn declare(&self, _width: u32, _height: u32, _scale: f32) {}

    pub fn configure(&mut self, ui: (u32, u32)) -> (u32, u32) {
        self.canvas.configured();
        if !self.canvas.covers(ui) {
            let (width, height) = resize_canvas(self.hwnd, ui.0, ui.1);
            if let Err(error) = self.rebuild(width, height) {
                warn!(target: "ui", "DXGI: could not grow the chain: {error:?}");
            }
        }
        // The UI texture always follows the window; only the chain's canvas
        // grows past it during a resize.
        ui
    }

    /// Gives the extra backbuffers back once the window's size has settled.
    pub fn settle(&mut self, ui: (u32, u32)) {
        if !self.canvas.settled() {
            return;
        }
        // A shrink must not land inside a drag: while the user is sizing the
        // window the desktop owns its frame, and `ResizeBuffers` there is what
        // was seen to take the device away. Wait for the drag to end.
        // SAFETY: `GetCapture` reads the calling thread's capture window.
        if !unsafe { windows::Win32::UI::Input::KeyboardAndMouse::GetCapture() }.is_invalid() {
            self.canvas.configured();
            return;
        }
        let target = idle_canvas(ui.0, ui.1);
        if self.canvas.size() == target {
            return;
        }
        if let Err(error) = self.rebuild(target.0, target.1) {
            warn!(target: "ui", "DXGI: could not shrink the chain: {error:?}");
        }
    }

    /// Acquires the backbuffer DXGI will present next and builds its frame.
    pub fn begin(&self, _ui: (u32, u32)) -> Option<Acquired> {
        let swapchain = self.swapchain.as_ref()?;
        if swapchain.is_removed() {
            return None;
        }
        let (width, height) = self.canvas.size();
        let index = usize::try_from(swapchain.current_index()).ok()?;
        let texture = *self.targets.get(index)?;
        let mut frame = pl::pl_frame {
            num_planes: 1,
            planes: [pl::pl_plane {
                texture,
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
        frame.crop = pl::pl_rect2df {
            x0: 0.0,
            y0: 0.0,
            x1: width as f32,
            y1: height as f32,
        };
        Some(Acquired {
            frame,
            width,
            height,
        })
    }

    /// Hands the backbuffer to the compositor.
    pub fn end(&self, _width: u32, _height: u32, _scale: f64) -> bool {
        let Some(swapchain) = self.swapchain.as_ref() else {
            return false;
        };
        swapchain.present().is_ok()
    }

    /// The inner renderer's failures are already logged; this one needs a line.
    pub fn present_failed(&self) {
        warn!(target: "ui", "DXGI present failed");
    }
}

impl Drop for Chain {
    fn drop(&mut self) {
        // The views name the chain's images, so they go before it. The
        // swapchain's own Drop releases the D3D12 resources.
        for mut target in self.targets.drain(..) {
            self.placebo.destroy_texture(&mut target);
        }
    }
}
