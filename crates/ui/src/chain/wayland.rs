//! The Wayland chain: libplacebo owns a `VkSwapchainKHR` on ZGUI's surface.
//!
//! The surface comes from ZGUI's raw Wayland handles and the swapchain is
//! created on the Vulkan device wgpu already opened. libplacebo renders into
//! the acquired image and presents it, so the interface costs one draw rather
//! than a draw into a wgpu surface plus a copy. The chain follows the surface's
//! extent exactly: Mesa's Wayland WSI scales an image that is a different size
//! from the surface instead of cropping it, which makes the Windows monitor-
//! sized canvas meaningless here.

// The Wayland chain is the no-op half of a cross-platform interface: its
// signatures have to match the Windows chain, where the same methods read a
// live swapchain and a per-display white point.
#![expect(
    clippy::unused_self,
    clippy::needless_pass_by_ref_mut,
    reason = "the methods are shared with the Windows chain, which uses both"
)]

use std::sync::Arc;

use ash::khr;
use ash::vk;
use ash::vk::Handle as _;
use eros::Context as _;
use libplacebo_sys as pl;
use raw_window_handle::HasDisplayHandle as _;
use raw_window_handle::HasWindowHandle as _;
use raw_window_handle::RawDisplayHandle;
use raw_window_handle::RawWindowHandle;
use tracing::warn;
use zgui::geom::Device;
use zgui::geom::Size;
use zgui::platform::GpuSurface;
use zgui::platform::Surface;
use zgui::surface::wgpu;
use zgui_render_wgpu::Gpu;

use super::Acquired;
use crate::placebo::Placebo;

/// The swapchain libplacebo presents with, and the handles it needs to live.
pub struct Chain {
    /// Keeps libvulkan loaded for the extension function pointers below.
    _entry: ash::Entry,
    surface: Arc<dyn Surface>,
    swapchain: pl::pl_swapchain,
    vk_surface: vk::SurfaceKHR,
    surface_ext: khr::surface::Instance,
    width: u32,
    height: u32,
}

impl Chain {
    pub fn new(
        gpu: &Arc<Gpu>,
        placebo: &Placebo,
        surface: &Arc<dyn Surface>,
        gpu_surface: &Arc<dyn GpuSurface>,
        width: u32,
        height: u32,
    ) -> eros::Result<Self> {
        // SAFETY: `ash::Entry::load` only resolves the Vulkan loader's symbols;
        // the resulting entry is owned by the chain and outlives every use.
        let entry = unsafe { ash::Entry::load() }.context("load the Vulkan loader")?;
        // SAFETY: `as_hal` only asks that the adapter stay alive while borrowed.
        let hal_adapter = unsafe { gpu.adapter().as_hal::<wgpu::hal::api::Vulkan>() }
            .context("the UI device is not Vulkan")?;
        let instance = hal_adapter.shared_instance().raw_instance();
        let physical = hal_adapter.raw_physical_device();

        let display = gpu_surface
            .display_handle()
            .map_err(|error| eros::error!("display handle: {error}"))?;
        let window = gpu_surface
            .window_handle()
            .map_err(|error| eros::error!("window handle: {error}"))?;
        let (wl_display, wl_surface) = match (display.as_raw(), window.as_raw()) {
            (RawDisplayHandle::Wayland(display), RawWindowHandle::Wayland(window)) => {
                (display.display.as_ptr(), window.surface.as_ptr())
            }
            _ => return Err(eros::error!("the surface is not a Wayland surface")),
        };

        let wayland_ext = khr::wayland_surface::Instance::new(&entry, instance);
        let create = vk::WaylandSurfaceCreateInfoKHR::default()
            .display(wl_display)
            .surface(wl_surface);
        // SAFETY: both pointers belong to the live ZGUI surface and the instance
        // is the one wgpu opened, which advertises VK_KHR_wayland_surface.
        let vk_surface = unsafe { wayland_ext.create_wayland_surface(&create, None) }
            .context("vkCreateWaylandSurfaceKHR")?;

        let surface_ext = khr::surface::Instance::new(&entry, instance);
        // SAFETY: the physical device and surface are live for the call.
        let modes =
            unsafe { surface_ext.get_physical_device_surface_present_modes(physical, vk_surface) }
                .context("vkGetPhysicalDeviceSurfacePresentModesKHR")?;
        let present_mode = [
            vk::PresentModeKHR::MAILBOX,
            vk::PresentModeKHR::FIFO_RELAXED,
            vk::PresentModeKHR::FIFO,
        ]
        .into_iter()
        .find(|mode| modes.contains(mode))
        .unwrap_or(vk::PresentModeKHR::FIFO);
        if present_mode != vk::PresentModeKHR::MAILBOX {
            warn!(target: "ui", "Wayland: mailbox unavailable; presenting in {present_mode:?} mode");
        }

        let params = pl::pl_vulkan_swapchain_params {
            surface: vk_surface.as_raw() as usize as *mut pl::VkSurfaceKHR_T,
            // SAFETY: `VkPresentModeKHR` is the same Vulkan enum in both bindings.
            present_mode: unsafe {
                std::mem::transmute::<i32, pl::VkPresentModeKHR>(present_mode.as_raw())
            },
            // Two frames in flight: libplacebo asks for `depth + 1` images, so
            // there is one displayed, one in the mailbox and one being drawn.
            swapchain_depth: 2,
            allow_suboptimal: true,
            ..Default::default()
        };
        // SAFETY: the surface was created on this instance and libplacebo owns
        // the imported device, which has VK_KHR_swapchain enabled by wgpu.
        let swapchain = unsafe { pl::pl_vulkan_create_swapchain(placebo.vulkan(), &params) };
        if swapchain.is_null() {
            // SAFETY: the surface was made above and is destroyed exactly once.
            unsafe { surface_ext.destroy_surface(vk_surface, None) };
            return Err(eros::error!(
                "libplacebo refused to create the Wayland swapchain"
            ));
        }

        let mut this = Self {
            _entry: entry,
            surface: Arc::clone(surface),
            swapchain,
            vk_surface,
            surface_ext,
            width,
            height,
        };
        this.resize(width, height);
        Ok(this)
    }

    pub const fn presentable(&self) -> bool {
        !self.swapchain.is_null()
    }

    pub const fn sdr_white(&self) -> f32 {
        // The desktop is SDR; the value only colours the interface overlay.
        200.0
    }

    /// Presentation is paced by libplacebo's Vulkan mailbox, not by ZGUI's
    /// surface callbacks. Every frame this renderer composes is submitted to
    /// the swapchain immediately; the compositor shows the newest one.
    ///
    /// `Surface::present_now()` must not be called here. Besides answering
    /// whether a callback is owed, it *marks* the frame as held in ZGUI's
    /// surface pacer — state only a renderer that finishes held frames through
    /// `present_held()` can release. This renderer presents at once, so a held
    /// mark leaves ZGUI refusing every later redraw until its 200 ms watchdog
    /// expires. By then the callback it waited for is one serial behind the one
    /// the watchdog probe just asked for, so it is ignored, the pacer stays
    /// owed, and the window is locked to a 200 ms cadence with half its frames
    /// dropped. `ComposeAhead::Always` on the window keeps ZGUI composing at
    /// the animation cadence while the mailbox decides what reaches the screen.
    pub const fn wait_for_frame(&self, _timeout_ms: u32) -> bool {
        true
    }

    /// The chain is the window: every configure changes its extent, and there
    /// is no larger canvas to settle back from.
    /// Declares a window extent before the swapchain has followed it.
    ///
    /// KWin decides where a maximized frame goes from the geometry declared at
    /// the start of the animation. Calling this on configure, with the extent
    /// the compositor just granted, keeps that geometry in step with the new
    /// surface; declaring the old swapchain extent centres the old window in
    /// the new one and leaves the frame offset by half the difference.
    pub fn declare(&self, width: u32, height: u32, scale: f32) {
        self.surface.pre_present_frame(
            Size::<i32, Device>::new(width as i32, height as i32),
            f64::from(scale),
        );
    }

    /// Resizes to the requested extent and reports the extent it actually got.
    /// A maximize configure can arrive before the surface itself grows, in
    /// which case libplacebo caps the swapchain at the old extent; the UI must
    /// be rebuilt for what was granted, not for what was asked for, or it is
    /// drawn large and scaled down into the smaller surface.
    pub fn configure(&mut self, ui: (u32, u32)) -> (u32, u32) {
        if (self.width, self.height) != ui {
            self.resize(ui.0, ui.1);
        }
        (self.width, self.height)
    }

    pub const fn settle(&mut self, _ui: (u32, u32)) {}

    pub fn resize(&mut self, width: u32, height: u32) -> bool {
        if !self.presentable() {
            return false;
        }
        let mut width = i32::try_from(width.max(1)).unwrap_or(i32::MAX);
        let mut height = i32::try_from(height.max(1)).unwrap_or(i32::MAX);
        // SAFETY: the swapchain is live; libplacebo writes back the extent it
        // actually created, which can differ from the request by surface caps.
        if !unsafe { pl::pl_swapchain_resize(self.swapchain, &mut width, &mut height) } {
            warn!(target: "ui", "Wayland: swapchain resize failed");
            return false;
        }
        self.width = u32::try_from(width).unwrap_or(1);
        self.height = u32::try_from(height).unwrap_or(1);
        true
    }

    /// Acquires the next image and turns it into a libplacebo frame.
    pub fn begin(&self, _ui: (u32, u32)) -> Option<Acquired> {
        if !self.presentable() {
            return None;
        }
        let mut sc_frame = pl::pl_swapchain_frame::default();
        // SAFETY: the swapchain is live and the out-parameter is a local.
        if !unsafe { pl::pl_swapchain_start_frame(self.swapchain, &mut sc_frame) } {
            return None;
        }
        let mut frame = pl::pl_frame::default();
        // SAFETY: `sc_frame` describes a live image from this swapchain, and
        // the frame borrows only its colour metadata and texture handle.
        unsafe { pl::pl_frame_from_swapchain(&mut frame, &sc_frame) };
        Some(Acquired {
            frame,
            width: self.width,
            height: self.height,
        })
    }

    /// Submits the render commands and commits the image to the surface.
    ///
    /// The extent passed in is the one the swapchain actually presented, which
    /// can lead the surface's own configure during a maximize. Declaring that
    /// extent here keeps `set_window_geometry` in step with the buffer; using
    /// the surface's stale size makes KWin centre an old-size window inside
    /// the new one and place the frame off-screen.
    pub fn end(&self, width: u32, height: u32, scale: f64) -> bool {
        if !self.presentable() {
            return false;
        }
        // SAFETY: a frame was acquired and rendered before this call.
        if !unsafe { pl::pl_swapchain_submit_frame(self.swapchain) } {
            warn!(target: "ui", "Wayland: swapchain submit failed");
            return false;
        }
        self.surface
            .pre_present_frame(Size::<i32, Device>::new(width as i32, height as i32), scale);
        // SAFETY: the submitted image is the one acquired by `begin`.
        unsafe { pl::pl_swapchain_swap_buffers(self.swapchain) };
        true
    }

    pub fn present_failed(&self) {
        warn!(target: "ui", "Wayland: present failed");
    }
}

impl Drop for Chain {
    fn drop(&mut self) {
        // SAFETY: the swapchain, instance and surface all came from the calls
        // above and are destroyed once, in reverse order of creation.
        unsafe {
            pl::pl_swapchain_destroy(&raw mut self.swapchain);
            self.surface_ext.destroy_surface(self.vk_surface, None);
        }
    }
}
