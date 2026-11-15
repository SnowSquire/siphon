//! A D3D12 swapchain whose backbuffers are imported into Vulkan and wgpu.
//!
//! The window is ZGUI's; this module only owns presentation. ZGUI composes its UI
//! into these textures through `renderer_supplied`, and `Present1` hands the filled
//! backbuffer to DXGI. There is no `VkSurfaceKHR` and no `VkSwapchainKHR`.

// Ported from the video spike: HDR paths stay for later; the UI presents SDR.
#![allow(dead_code)]
use std::ffi::c_void;
use std::mem::size_of;

use anyhow::Context as _;
use anyhow::anyhow;
use ash::khr;
use ash::vk;
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::Foundation::FALSE;
use windows::Win32::Foundation::GENERIC_ALL;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Foundation::HWND;
use windows::Win32::Foundation::LUID;
use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_1;
use windows::Win32::Graphics::Direct3D12::D3D12_COMMAND_LIST_TYPE_DIRECT;
use windows::Win32::Graphics::Direct3D12::D3D12_COMMAND_QUEUE_DESC;
use windows::Win32::Graphics::Direct3D12::D3D12_COMMAND_QUEUE_FLAG_NONE;
use windows::Win32::Graphics::Direct3D12::D3D12_COMMAND_QUEUE_PRIORITY_NORMAL;
use windows::Win32::Graphics::Direct3D12::D3D12CreateDevice;
use windows::Win32::Graphics::Direct3D12::ID3D12CommandQueue;
use windows::Win32::Graphics::Direct3D12::ID3D12Device;
use windows::Win32::Graphics::Direct3D12::ID3D12DeviceChild;
use windows::Win32::Graphics::Direct3D12::ID3D12Resource;
use windows::Win32::Graphics::Dxgi::Common::DXGI_ALPHA_MODE_IGNORE;
use windows::Win32::Graphics::Dxgi::Common::DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709;
use windows::Win32::Graphics::Dxgi::Common::DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R10G10B10A2_UNORM;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_UNKNOWN;
use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;
use windows::Win32::Graphics::Dxgi::CreateDXGIFactory2;
use windows::Win32::Graphics::Dxgi::DXGI_CREATE_FACTORY_FLAGS;
use windows::Win32::Graphics::Dxgi::DXGI_HDR_METADATA_HDR10;
use windows::Win32::Graphics::Dxgi::DXGI_HDR_METADATA_TYPE_HDR10;
use windows::Win32::Graphics::Dxgi::DXGI_MWA_NO_ALT_ENTER;
use windows::Win32::Graphics::Dxgi::DXGI_PRESENT;
use windows::Win32::Graphics::Dxgi::DXGI_PRESENT_PARAMETERS;
use windows::Win32::Graphics::Dxgi::DXGI_RGBA;
use windows::Win32::Graphics::Dxgi::DXGI_SCALING_NONE;
use windows::Win32::Graphics::Dxgi::DXGI_SWAP_CHAIN_COLOR_SPACE_SUPPORT_FLAG_PRESENT;
use windows::Win32::Graphics::Dxgi::DXGI_SWAP_CHAIN_DESC1;
use windows::Win32::Graphics::Dxgi::DXGI_SWAP_CHAIN_FLAG;
use windows::Win32::Graphics::Dxgi::DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING;
use windows::Win32::Graphics::Dxgi::DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT;
use windows::Win32::Graphics::Dxgi::DXGI_SWAP_EFFECT_FLIP_DISCARD;
use windows::Win32::Graphics::Dxgi::DXGI_USAGE_RENDER_TARGET_OUTPUT;
use windows::Win32::Graphics::Dxgi::IDXGIAdapter4;
use windows::Win32::Graphics::Dxgi::IDXGIFactory7;
use windows::Win32::Graphics::Dxgi::IDXGIOutput;
use windows::Win32::Graphics::Dxgi::IDXGIOutput6;
use windows::Win32::Graphics::Dxgi::IDXGISwapChain2;
use windows::Win32::Graphics::Dxgi::IDXGISwapChain3;
use windows::Win32::Graphics::Dxgi::IDXGISwapChain4;
use windows::Win32::System::Threading::WaitForSingleObject;
use windows::core::Interface as _;
use windows::core::PCWSTR;
use zgui::surface::wgpu;
use zgui_render_wgpu::Gpu;

/// Three buffers: the compositor can hold one while the GPU renders into the next
/// and the third stays free, which is the configuration the flip-model guides
/// recommend and what keeps `Present` from stalling on a queue of one.
pub const BUFFERS: u32 = 3;

/// The Vulkan spelling of the 10-bit swapchain format.
const VK_FORMAT: vk::Format = vk::Format::A2B10G10R10_UNORM_PACK32;

/// What to do with HDR.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HdrMode {
    /// Enable HDR when the display is already in HDR mode.
    #[default]
    Auto,
    /// Require HDR; fail if the display cannot present PQ/Rec.2020.
    On,
    /// Present SDR.
    Off,
}

/// The chain's extent while the window's size is settled.
///
/// The UI is composed for the window's exact extent and presented 1:1 at the
/// top-left of the chain, so at rest the chain only has to be as large as the
/// window itself. This is the extent it holds whenever nothing is moving.
#[must_use]
pub fn idle_canvas(width: u32, height: u32) -> (u32, u32) {
    match chain_mode() {
        // `half` exists to expose the compositor's fill outside the back buffer,
        // so it has to stay smaller than the window even at rest.
        ChainMode::Half => (width.max(2) / 2, height.max(2) / 2),
        ChainMode::Exact | ChainMode::Canvas => (width.max(1), height.max(1)),
    }
}

/// The chain's extent while a resize is in flight.
///
/// A chain at the window's own extent pays `ResizeBuffers` — 3 to 6 ms at
/// 320 Hz — on every event of a drag. A chain covering the monitor the window is
/// on pays it once when the drag starts, and once more when the size settles;
/// every event in between is a repaint of the interface texture alone, which is
/// what makes the content reflow at the display's rate.
#[must_use]
pub fn resize_canvas(hwnd: HWND, width: u32, height: u32) -> (u32, u32) {
    match chain_mode() {
        ChainMode::Exact | ChainMode::Half => idle_canvas(width, height),
        ChainMode::Canvas => {
            let (monitor_width, monitor_height) = monitor_size(hwnd);
            (
                monitor_width.max(width.max(1)),
                monitor_height.max(height.max(1)),
            )
        }
    }
}

/// The monitor the window is on, or the virtual screen if the desktop will not
/// say. A window dragged onto another monitor grows the chain to that one.
fn monitor_size(hwnd: HWND) -> (u32, u32) {
    use windows::Win32::Graphics::Gdi::GetMonitorInfoW;
    use windows::Win32::Graphics::Gdi::MONITOR_DEFAULTTONEAREST;
    use windows::Win32::Graphics::Gdi::MONITORINFO;
    use windows::Win32::Graphics::Gdi::MonitorFromWindow;

    // SAFETY: `hwnd` is this process's window and the fallback is a documented
    // flag; the call takes no pointers out of this frame.
    let monitor = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
    if !monitor.is_invalid() {
        let mut info = MONITORINFO {
            cbSize: u32::try_from(size_of::<MONITORINFO>()).unwrap_or(0),
            ..Default::default()
        };
        // SAFETY: `monitor` names a monitor and `info` declares its own size.
        if unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
            let rect = info.rcMonitor;
            let width = u32::try_from(rect.right - rect.left).unwrap_or(0);
            let height = u32::try_from(rect.bottom - rect.top).unwrap_or(0);
            if width > 0 && height > 0 {
                return (width, height);
            }
        }
    }
    virtual_screen()
}

/// Every monitor at once, for a window whose monitor the desktop will not name.
fn virtual_screen() -> (u32, u32) {
    use windows_sys::Win32::UI::WindowsAndMessaging::GetSystemMetrics;
    use windows_sys::Win32::UI::WindowsAndMessaging::SM_CXVIRTUALSCREEN;
    use windows_sys::Win32::UI::WindowsAndMessaging::SM_CYVIRTUALSCREEN;

    // SAFETY: the metric indices are the two documented ones; GetSystemMetrics
    // takes no pointers and fails by returning 0, which `max(1)` covers.
    let (width, height) = unsafe {
        (
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    };
    (
        u32::try_from(width).unwrap_or(1).max(1),
        u32::try_from(height).unwrap_or(1).max(1),
    )
}

/// How the chain is sized against the window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ChainMode {
    /// The window's own extent at rest, the monitor's while it is being sized.
    Canvas,
    /// A chain at the window's own extent; `SIPHON_UI_CHAIN=exact`.
    Exact,
    /// Half the window; `SIPHON_UI_CHAIN=half`. A diagnostic that makes the
    /// compositor's fill outside the back buffer, and the colour it uses, visible.
    Half,
}

/// Which presentation the chain uses.
///
/// Read once: the environment cannot change under a running process, and the
/// answer is asked for on every resize event.
fn chain_mode() -> ChainMode {
    static MODE: std::sync::OnceLock<ChainMode> = std::sync::OnceLock::new();
    *MODE.get_or_init(|| match std::env::var("SIPHON_UI_CHAIN").as_deref() {
        Ok("exact") => ChainMode::Exact,
        Ok("half") => ChainMode::Half,
        _ => ChainMode::Canvas,
    })
}

/// The D3D12 device, queue and factory a window's swapchains share.
///
/// Creating them costs hundreds of milliseconds, and a rebuild for a new
/// extent needs neither a new device nor a new queue: the swapchain is the
/// only thing that has to be made again.
pub struct D3d12 {
    factory: IDXGIFactory7,
    device: ID3D12Device,
    queue: ID3D12CommandQueue,
}

impl D3d12 {
    /// Opens the D3D12 device for the adapter `gpu` runs on.
    pub fn new(gpu: &Gpu) -> anyhow::Result<Self> {
        // SAFETY: `as_hal` only asks that the resource behind the guard outlives the
        // borrow. The adapter handle is alive for the whole call.
        let hal_adapter = unsafe { gpu.adapter().as_hal::<wgpu::hal::api::Vulkan>() }
            .context("the video device is not Vulkan")?;
        let luid = vulkan_luid(&hal_adapter)?;
        // SAFETY: no pointers are passed; the factory is created on the default adapter set.
        let factory: IDXGIFactory7 = unsafe { CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)) }
            .context("CreateDXGIFactory2")?;
        // SAFETY: the factory is alive and `luid` is plain data.
        let adapter: IDXGIAdapter4 = unsafe { factory.EnumAdapterByLuid(luid) }
            .context("no D3D12 adapter for the Vulkan LUID")?;

        let mut device: Option<ID3D12Device> = None;
        // SAFETY: the adapter is alive and the out-pointer names a live local.
        unsafe { D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_11_1, &mut device) }
            .context("D3D12CreateDevice")?;
        let device = device.context("D3D12CreateDevice returned no device")?;

        let queue_desc = D3D12_COMMAND_QUEUE_DESC {
            Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
            Priority: D3D12_COMMAND_QUEUE_PRIORITY_NORMAL.0,
            Flags: D3D12_COMMAND_QUEUE_FLAG_NONE,
            NodeMask: 0,
        };
        // SAFETY: the device is alive and the descriptor is a valid local.
        let queue: ID3D12CommandQueue =
            unsafe { device.CreateCommandQueue(&queue_desc) }.context("CreateCommandQueue")?;

        Ok(Self {
            factory,
            device,
            queue,
        })
    }
}

/// One D3D12 swapchain and the Vulkan images that stand for its backbuffers.
pub struct DxgiSwapchain {
    /// The Vulkan images and memories libplacebo renders into.
    images: Vec<(vk::Image, vk::DeviceMemory)>,
    /// The NT handles the Vulkan memories were imported from.
    shared_handles: Vec<HANDLE>,
    /// The D3D12 resources themselves, kept alive while the Vulkan images exist.
    backbuffers: Vec<ID3D12Resource>,
    swapchain: IDXGISwapChain3,
    /// Signalled when there is room for another frame in the flip queue.
    latency: HANDLE,
    /// The `vk::Device` the images and memories were created on.
    device_handle: ash::Device,
    format: wgpu::TextureFormat,
    width: u32,
    height: u32,
    hdr: bool,
    max_luminance: f32,
    sdr_white: f32,
}

impl DxgiSwapchain {
    /// Builds a swapchain for `hwnd` on the same physical adapter as `gpu`.
    #[allow(
        clippy::borrow_as_ptr,
        reason = "the Windows bindings take pointers to descriptors"
    )]
    pub fn new(
        d3d12: &D3d12,
        gpu: &Gpu,
        hwnd: HWND,
        width: u32,
        height: u32,
        hdr: HdrMode,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(width > 0 && height > 0, "zero-sized swapchain");

        // SAFETY: `as_hal` only asks that the resource behind the guard outlives the
        // borrow. The device handle is alive for the whole call.
        let hal_device = unsafe { gpu.device().as_hal::<wgpu::hal::api::Vulkan>() }
            .context("the video device is not Vulkan")?;

        let enabled = hal_device.enabled_device_extensions();
        anyhow::ensure!(
            enabled.contains(&khr::external_memory_win32::NAME),
            "the Vulkan device does not have VK_KHR_external_memory_win32"
        );

        let factory = &d3d12.factory;
        let queue = &d3d12.queue;

        let desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: width,
            Height: height,
            Format: DXGI_FORMAT_R10G10B10A2_UNORM,
            Stereo: FALSE,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: BUFFERS,
            // Native: while a drag moves, the chain is a canvas larger than
            // the window and the compositor shows its top-left corner 1:1.
            Scaling: DXGI_SCALING_NONE,
            // Discard, not sequential: every frame is drawn from scratch, so
            // preserving the backbuffer's contents buys nothing and rules out
            // the independent-flip paths that variable refresh needs.
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
            AlphaMode: DXGI_ALPHA_MODE_IGNORE,
            Flags: (DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING.0
                | DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT.0) as u32,
        };
        // SAFETY: the queue and HWND are alive, and the descriptor is a valid local.
        let swapchain1 = unsafe {
            factory.CreateSwapChainForHwnd(queue, hwnd, &desc, None, None::<&IDXGIOutput>)
        }
        .context("CreateSwapChainForHwnd")?;
        // Whatever `DXGI_SCALING_NONE` leaves outside the back buffer — a band of
        // the client area a compositor has not taken from the chain yet, say — is
        // filled with the chain's background, and that defaults to opaque black.
        // The page colour is the only thing that may be behind the interface.
        // `SIPHON_UI_BACKGROUND=black` leaves the default in place, which is the
        // black this fill was before.
        let page = DXGI_RGBA {
            r: 18.0 / 255.0,
            g: 20.0 / 255.0,
            b: 26.0 / 255.0,
            a: 1.0,
        };
        let raw_black = std::env::var("SIPHON_UI_BACKGROUND").is_ok_and(|value| value == "black");
        if !raw_black {
            // SAFETY: the swapchain is alive and `page` is a valid local.
            unsafe { swapchain1.SetBackgroundColor(&page) }.context("SetBackgroundColor")?;
        }
        let swapchain: IDXGISwapChain3 = swapchain1.cast().context("swapchain vtable")?;
        let swapchain2: IDXGISwapChain2 = swapchain1.cast().context("swapchain vtable")?;
        // Two frames outstanding at most; the waitable object below is what
        // makes that a wait instead of a stall.
        // SAFETY: the swapchain is alive and nothing has been presented yet.
        unsafe { swapchain2.SetMaximumFrameLatency(2) }.context("SetMaximumFrameLatency")?;
        // SAFETY: the swapchain is alive; the handle is closed in `Drop`.
        let latency = unsafe { swapchain2.GetFrameLatencyWaitableObject() };
        // SAFETY: the factory and HWND are alive for the call.
        unsafe { factory.MakeWindowAssociation(hwnd, DXGI_MWA_NO_ALT_ENTER) }
            .context("MakeWindowAssociation")?;

        let (hdr, max_luminance, sdr_white) = configure_hdr(&swapchain, hdr)?;
        let format = wgpu::TextureFormat::Rgb10a2Unorm;

        let mut this = Self {
            images: Vec::with_capacity(BUFFERS as usize),
            shared_handles: Vec::with_capacity(BUFFERS as usize),
            backbuffers: Vec::with_capacity(BUFFERS as usize),
            swapchain,
            latency,
            device_handle: hal_device.raw_device().clone(),
            format,
            width,
            height,
            hdr,
            max_luminance,
            sdr_white,
        };
        this.attach_backbuffers(d3d12, gpu, width, height)?;
        Ok(this)
    }

    /// Replaces the backbuffers with ones of a new extent on the same chain.
    ///
    /// The caller has already destroyed every libplacebo texture over the old
    /// images; this releases the D3D12 resources the old buffer set held, so
    /// `ResizeBuffers` sees the last reference go.
    pub fn resize(
        &mut self,
        d3d12: &D3d12,
        gpu: &Gpu,
        width: u32,
        height: u32,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(width > 0 && height > 0, "zero-sized swapchain");
        self.release_buffers();
        let started = std::time::Instant::now();
        // SAFETY: every reference to the old buffers has been dropped, and the
        // flags are the ones the chain was created with.
        unsafe {
            self.swapchain.ResizeBuffers(
                BUFFERS,
                width,
                height,
                DXGI_FORMAT_UNKNOWN,
                DXGI_SWAP_CHAIN_FLAG(
                    DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING.0
                        | DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT.0,
                ),
            )
        }
        .context("ResizeBuffers")?;
        tracing::debug!(
            target: "ui",
            width,
            height,
            elapsed = ?started.elapsed(),
            "resize buffers"
        );
        self.attach_backbuffers(d3d12, gpu, width, height)?;
        self.width = width;
        self.height = height;
        // The colour space, HDR metadata and SDR white point are decisions
        // about the display and survive a resize; only a window that moved to
        // another output changes them, which is the next feature's problem.
        Ok(())
    }

    /// Imports the chain's current backbuffers into Vulkan.
    fn attach_backbuffers(
        &mut self,
        d3d12: &D3d12,
        gpu: &Gpu,
        width: u32,
        height: u32,
    ) -> anyhow::Result<()> {
        // SAFETY: `as_hal` only asks that the resource behind the guard outlives the
        // borrow. Both wgpu handles are alive for the whole call.
        let hal_adapter = unsafe { gpu.adapter().as_hal::<wgpu::hal::api::Vulkan>() }
            .context("the video device is not Vulkan")?;
        // SAFETY: as above.
        let hal_device = unsafe { gpu.device().as_hal::<wgpu::hal::api::Vulkan>() }
            .context("the video device is not Vulkan")?;
        let instance = hal_adapter.shared_instance().raw_instance();

        for index in 0..BUFFERS {
            // SAFETY: the swapchain is alive and `index` is below its buffer count.
            let resource: ID3D12Resource = unsafe { self.swapchain.GetBuffer(index) }
                .with_context(|| format!("GetBuffer({index})"))?;
            let child: ID3D12DeviceChild = resource.cast().context("backbuffer child")?;
            // SAFETY: the resource is a D3D12 texture, the handle is an NT handle, and
            // `GENERIC_ALL` is what the reference DXGI interop path uses.
            let handle = unsafe {
                d3d12
                    .device
                    .CreateSharedHandle(&child, None, GENERIC_ALL.0, PCWSTR::null())
            }
            .with_context(|| format!("CreateSharedHandle({index})"))?;

            // Kept before the import so a failed import is still cleaned up by
            // `release_buffers` rather than leaking the handle and resource.
            self.shared_handles.push(handle);
            self.backbuffers.push(resource);
            // SAFETY: the image is created on the same device and memory is bound to it
            // below; `release_buffers` destroys both exactly once.
            let imported = unsafe {
                import_backbuffer(&hal_device, instance, handle, width, height, VK_FORMAT)
            }
            .with_context(|| format!("import backbuffer {index}"))?;
            self.images.push(imported);
        }
        Ok(())
    }

    /// Destroys the Vulkan images and closes everything that names a backbuffer.
    fn release_buffers(&mut self) {
        for (image, memory) in self.images.drain(..) {
            // SAFETY: both came from `device_handle` and are freed once.
            unsafe {
                self.device_handle.destroy_image(image, None);
                self.device_handle.free_memory(memory, None);
            }
        }
        for handle in self.shared_handles.drain(..) {
            // SAFETY: every handle came from `CreateSharedHandle` and is closed once.
            unsafe {
                let _ = CloseHandle(handle);
            }
        }
        self.backbuffers.clear();
    }

    /// The backbuffer DXGI will present next.
    #[must_use]
    pub fn current_index(&self) -> u32 {
        // SAFETY: the swapchain is alive for the borrow.
        unsafe { self.swapchain.GetCurrentBackBufferIndex() }
    }

    /// The Vulkan image standing for backbuffer `index`.
    #[must_use]
    pub fn image(&self, index: u32) -> Option<vk::Image> {
        Some(self.images.get(usize::try_from(index).ok()?)?.0)
    }

    /// The pixel size the swapchain was created with.
    #[must_use]
    pub const fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// The format every backbuffer uses.
    #[must_use]
    pub const fn format(&self) -> wgpu::TextureFormat {
        self.format
    }

    /// Whether the swapchain is presenting PQ/Rec.2020.
    #[must_use]
    pub const fn hdr(&self) -> bool {
        self.hdr
    }

    /// The display's reported peak luminance in nits, or zero when unknown.
    #[must_use]
    pub const fn max_luminance(&self) -> f32 {
        self.max_luminance
    }

    /// The display's SDR white point in nits.
    #[must_use]
    pub const fn sdr_white(&self) -> f32 {
        self.sdr_white
    }

    /// Waits until the flip queue has room for another frame.
    ///
    /// Returns false on timeout, which means the compositor is not consuming
    /// frames (occluded, minimized, or a stalled display); the caller can still
    /// render and present, it just cannot be sure the queue is free.
    pub fn wait_for_frame(&self, timeout_ms: u32) -> bool {
        // SAFETY: the handle came from this swapchain and lives in `self`.
        let result = unsafe { WaitForSingleObject(self.latency, timeout_ms) };
        result == windows::Win32::Foundation::WAIT_OBJECT_0
    }

    /// Hands the current backbuffer to the compositor.
    #[allow(
        clippy::borrow_as_ptr,
        reason = "the Windows bindings take a pointer to the present parameters"
    )]
    pub fn present(&self) -> anyhow::Result<()> {
        let params = DXGI_PRESENT_PARAMETERS::default();
        // SAFETY: the swapchain lives in `self`; the parameters describe the whole frame
        // (no dirty rects), which is what a flip-model present expects.
        let result = unsafe { self.swapchain.Present1(1, DXGI_PRESENT(0), &params) };
        result.ok().context("Present1")
    }
}

impl Drop for DxgiSwapchain {
    fn drop(&mut self) {
        // The libplacebo textures over these images are destroyed by the renderer
        // before the swapchain drops, so nothing can still be rendering into them.
        self.release_buffers();
        // SAFETY: the handle came from `GetFrameLatencyWaitableObject`, names
        // nothing else, and is closed once.
        unsafe {
            let _ = CloseHandle(self.latency);
        }
    }
}

/// Reads the LUID of the Vulkan physical device so DXGI can find the same adapter.
fn vulkan_luid(adapter: &wgpu::hal::vulkan::Adapter) -> anyhow::Result<LUID> {
    let instance = adapter.shared_instance().raw_instance();
    let physical = adapter.raw_physical_device();
    let mut id = vk::PhysicalDeviceIDProperties::default();
    let mut properties = vk::PhysicalDeviceProperties2::default().push_next(&mut id);
    // SAFETY: the instance and physical device are live for the borrow; `properties`
    // and the chained `id` outlive the call.
    unsafe {
        instance.get_physical_device_properties2(physical, &mut properties);
    }
    anyhow::ensure!(
        id.device_luid_valid != vk::FALSE,
        "the Vulkan device has no LUID"
    );
    let low = u32::from_ne_bytes(id.device_luid[0..4].try_into().expect("four bytes"));
    let high = i32::from_ne_bytes(id.device_luid[4..8].try_into().expect("four bytes"));
    Ok(LUID {
        LowPart: low,
        HighPart: high,
    })
}

/// Puts the swapchain in HDR when requested and available.
///
/// Also answers where the user put SDR white, which is what the interface and
/// any SDR video have to be rendered against.
fn configure_hdr(swapchain: &IDXGISwapChain3, mode: HdrMode) -> anyhow::Result<(bool, f32, f32)> {
    // SAFETY: the swapchain is alive for the call.
    let output = unsafe { swapchain.GetContainingOutput() }.context("GetContainingOutput")?;
    let output6: IDXGIOutput6 = output.cast().context("IDXGIOutput6")?;
    // SAFETY: `output6` is a live output.
    let desc = unsafe { output6.GetDesc1() }.context("GetDesc1")?;
    // SAFETY: the swapchain is alive for the call.
    let support =
        unsafe { swapchain.CheckColorSpaceSupport(DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020) }
            .context("CheckColorSpaceSupport")?;
    let supported = support & (DXGI_SWAP_CHAIN_COLOR_SPACE_SUPPORT_FLAG_PRESENT.0 as u32) != 0;
    eprintln!(
        "DXGI output: color space {:?}, peak {} nits, max full-frame {} nits, bits {}",
        desc.ColorSpace, desc.MaxLuminance, desc.MaxFullFrameLuminance, desc.BitsPerColor
    );

    let active = match mode {
        HdrMode::Off => false,
        HdrMode::On => {
            anyhow::ensure!(supported, "the display does not support PQ/Rec.2020");
            true
        }
        HdrMode::Auto => supported && desc.ColorSpace == DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020,
    };

    let space = if active {
        DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020
    } else {
        DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709
    };
    // SAFETY: the swapchain is alive and `space` is one it just reported supporting
    // (or the default SDR space).
    unsafe { swapchain.SetColorSpace1(space) }.context("SetColorSpace1")?;
    if active {
        set_hdr_metadata(swapchain)?;
    }
    let sdr_white = match crate::sdr::white_nits(&desc.DeviceName) {
        Ok(nits) => nits,
        Err(error) => {
            eprintln!("SDR white: {error:#}; assuming 200 nits");
            200.0
        }
    };
    eprintln!("SDR white: {sdr_white:.0} nits (level from the display configuration)");
    Ok((active, desc.MaxLuminance, sdr_white))
}

/// Publishes the mastering metadata for the HDR10 container.
fn set_hdr_metadata(swapchain: &IDXGISwapChain3) -> anyhow::Result<()> {
    let swapchain4: IDXGISwapChain4 = swapchain.cast().context("IDXGISwapChain4")?;
    let metadata = DXGI_HDR_METADATA_HDR10 {
        // Rec.2020 primaries and D65 white, in 1/50000 units.
        RedPrimary: [35400, 14600],
        GreenPrimary: [8500, 39850],
        BluePrimary: [6550, 2300],
        WhitePoint: [15635, 16450],
        // 1000 nits mastering, 0.005 nits black, 1000/400 nits content.
        MaxMasteringLuminance: 1000,
        MinMasteringLuminance: 50,
        MaxContentLightLevel: 1000,
        MaxFrameAverageLightLevel: 400,
    };
    // SAFETY: the struct is plain data and the slice covers exactly it.
    let bytes = unsafe {
        std::slice::from_raw_parts(
            (&raw const metadata).cast::<u8>(),
            size_of::<DXGI_HDR_METADATA_HDR10>(),
        )
    };
    // SAFETY: the swapchain is alive and `bytes` outlives the call.
    unsafe { swapchain4.SetHDRMetaData(DXGI_HDR_METADATA_TYPE_HDR10, Some(bytes)) }
        .context("SetHDRMetaData")
}

/// Creates a Vulkan image over a D3D12 backbuffer's shared memory.
///
/// # Safety
///
/// `handle` must be an NT handle from `ID3D12Device::CreateSharedHandle` for a
/// texture matching `width` and `height`, and `device` must be the Vulkan device
/// the handle was created for.
unsafe fn import_backbuffer(
    device: &wgpu::hal::vulkan::Device,
    instance: &ash::Instance,
    handle: HANDLE,
    width: u32,
    height: u32,
    vk_format: vk::Format,
) -> anyhow::Result<(vk::Image, vk::DeviceMemory)> {
    let raw = device.raw_device();
    let physical = device.raw_physical_device();
    let win32 = khr::external_memory_win32::Device::new(instance, raw);

    let mut external = vk::ExternalMemoryImageCreateInfo::default()
        .handle_types(vk::ExternalMemoryHandleTypeFlags::D3D12_RESOURCE);
    let image_info = vk::ImageCreateInfo::default()
        .image_type(vk::ImageType::TYPE_2D)
        .format(vk_format)
        .extent(vk::Extent3D {
            width,
            height,
            depth: 1,
        })
        .mip_levels(1)
        .array_layers(1)
        .samples(vk::SampleCountFlags::TYPE_1)
        .tiling(vk::ImageTiling::OPTIMAL)
        // The same usages a D3D12 backbuffer resource has: render target, copy
        // source/destination and shader resource. libplacebo picks its render
        // path from these.
        .usage(
            vk::ImageUsageFlags::COLOR_ATTACHMENT
                | vk::ImageUsageFlags::TRANSFER_SRC
                | vk::ImageUsageFlags::TRANSFER_DST
                | vk::ImageUsageFlags::SAMPLED,
        )
        .sharing_mode(vk::SharingMode::EXCLUSIVE)
        .initial_layout(vk::ImageLayout::UNDEFINED)
        .push_next(&mut external);

    // SAFETY: the caller promises the handle names a texture of this shape and device.
    let image = unsafe { raw.create_image(&image_info, None) }.context("vkCreateImage")?;
    // SAFETY: `image` was created on this device and is still alive.
    let requirements = unsafe { raw.get_image_memory_requirements(image) };

    let mut handle_properties = vk::MemoryWin32HandlePropertiesKHR::default();
    // SAFETY: `win32` was made for this device and `handle` is the caller's NT handle.
    unsafe {
        win32.get_memory_win32_handle_properties(
            vk::ExternalMemoryHandleTypeFlags::D3D12_RESOURCE,
            handle.0 as vk::HANDLE,
            &mut handle_properties,
        )
    }
    .context("vkGetMemoryWin32HandlePropertiesKHR")?;

    // SAFETY: the physical device belongs to the instance and is still alive.
    let memory_properties = unsafe { instance.get_physical_device_memory_properties(physical) };
    let candidates = handle_properties.memory_type_bits & requirements.memory_type_bits;
    anyhow::ensure!(
        candidates != 0,
        "no Vulkan memory type for the D3D12 resource"
    );
    let memory_type = (0..memory_properties.memory_type_count)
        .find(|index| {
            candidates & (1 << index) != 0
                && memory_properties.memory_types[*index as usize]
                    .property_flags
                    .contains(vk::MemoryPropertyFlags::DEVICE_LOCAL)
        })
        .or_else(|| {
            (0..memory_properties.memory_type_count).find(|index| candidates & (1 << index) != 0)
        })
        .context("no usable Vulkan memory type")?;

    let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
    let mut import_info = vk::ImportMemoryWin32HandleInfoKHR::default()
        .handle_type(vk::ExternalMemoryHandleTypeFlags::D3D12_RESOURCE)
        .handle(handle.0 as vk::HANDLE);
    let allocation = vk::MemoryAllocateInfo::default()
        .allocation_size(requirements.size)
        .memory_type_index(memory_type)
        .push_next(&mut import_info)
        .push_next(&mut dedicated);

    // SAFETY: the import info names the handle and the dedicated image, and the size
    // and type satisfy the image's requirements.
    let memory = unsafe { raw.allocate_memory(&allocation, None) }.context("vkAllocateMemory")?;
    // SAFETY: `memory` was just allocated for `image` with a dedicated binding.
    if let Err(error) = unsafe { raw.bind_image_memory(image, memory, 0) } {
        // SAFETY: both objects are ours and nothing else names them.
        unsafe {
            raw.destroy_image(image, None);
            raw.free_memory(memory, None);
        }
        return Err(error).context("vkBindImageMemory");
    }

    Ok((image, memory))
}

/// Converts a raw handle into the Windows `HWND` type.
pub fn hwnd_from_raw(handle: raw_window_handle::RawWindowHandle) -> anyhow::Result<HWND> {
    let raw_window_handle::RawWindowHandle::Win32(win32) = handle else {
        return Err(anyhow!("the surface is not a Win32 window"));
    };
    Ok(HWND(win32.hwnd.get() as *mut c_void))
}

/// Colours everything behind the interface.
///
/// The window class the platform registers has no background brush, so any pixel
/// the compositor has to fill rather than take from the chain is black: the area
/// `DXGI_SCALING_NONE` leaves outside the back buffer, whatever DWM exposes while
/// a window is being resized, and `WM_ERASEBKGND` if it is ever sent. The page's
/// own colour is the only thing that may be behind the interface, and with it a
/// frame that exposes that area shows the page instead of a black bar.
pub fn set_window_background(hwnd: HWND) {
    use windows::Win32::Foundation::COLORREF;
    use windows::Win32::Graphics::Gdi::CreateSolidBrush;
    use windows::Win32::UI::WindowsAndMessaging::GCLP_HBRBACKGROUND;
    use windows::Win32::UI::WindowsAndMessaging::SetClassLongPtrW;

    // The page background, `#12141a`, as a `0x00BBGGRR` colour reference.
    const PAGE: COLORREF = COLORREF(0x001a_1412);
    // SAFETY: a solid brush takes no pointers and fails by returning null, which
    // `SetClassLongPtrW` accepts as "no brush" — the state it is in already.
    let brush = unsafe { CreateSolidBrush(PAGE) };
    // SAFETY: `hwnd` is this window's, the class is alive as long as it is, and
    // the brush is deliberately never deleted because the class keeps the only
    // reference to it.
    unsafe {
        SetClassLongPtrW(hwnd, GCLP_HBRBACKGROUND, brush.0 as isize);
    }
}
