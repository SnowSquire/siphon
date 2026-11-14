//! A D3D12 swapchain whose backbuffers are imported into Vulkan and wgpu.
//!
//! The window is ZGUI's; this module only owns presentation. ZGUI composes its UI
//! into these textures through `renderer_supplied`, and `Present1` hands the filled
//! backbuffer to DXGI. There is no `VkSurfaceKHR` and no `VkSwapchainKHR`.

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
use windows::Win32::Graphics::Direct3D12::D3D12CreateDevice;
use windows::Win32::Graphics::Direct3D12::D3D12_COMMAND_LIST_TYPE_DIRECT;
use windows::Win32::Graphics::Direct3D12::D3D12_COMMAND_QUEUE_DESC;
use windows::Win32::Graphics::Direct3D12::D3D12_COMMAND_QUEUE_FLAG_NONE;
use windows::Win32::Graphics::Direct3D12::D3D12_COMMAND_QUEUE_PRIORITY_NORMAL;
use windows::Win32::Graphics::Direct3D12::ID3D12CommandQueue;
use windows::Win32::Graphics::Direct3D12::ID3D12Device;
use windows::Win32::Graphics::Direct3D12::ID3D12DeviceChild;
use windows::Win32::Graphics::Direct3D12::ID3D12Resource;
use windows::Win32::Graphics::Dxgi::Common::DXGI_ALPHA_MODE_IGNORE;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R10G10B10A2_UNORM;
use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;
use windows::Win32::Graphics::Dxgi::Common::DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020;
use windows::Win32::Graphics::Dxgi::Common::DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709;
use windows::Win32::Graphics::Dxgi::CreateDXGIFactory2;
use windows::Win32::Graphics::Dxgi::DXGI_CREATE_FACTORY_FLAGS;
use windows::Win32::Graphics::Dxgi::DXGI_HDR_METADATA_HDR10;
use windows::Win32::Graphics::Dxgi::DXGI_HDR_METADATA_TYPE_HDR10;
use windows::Win32::Graphics::Dxgi::DXGI_MWA_NO_ALT_ENTER;
use windows::Win32::Graphics::Dxgi::DXGI_PRESENT;
use windows::Win32::Graphics::Dxgi::DXGI_PRESENT_PARAMETERS;
use windows::Win32::Graphics::Dxgi::DXGI_SCALING_NONE;
use windows::Win32::Graphics::Dxgi::DXGI_SWAP_CHAIN_DESC1;
use windows::Win32::Graphics::Dxgi::DXGI_SWAP_CHAIN_COLOR_SPACE_SUPPORT_FLAG_PRESENT;
use windows::Win32::Graphics::Dxgi::DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL;
use windows::Win32::Graphics::Dxgi::DXGI_USAGE_RENDER_TARGET_OUTPUT;
use windows::Win32::Graphics::Dxgi::IDXGIAdapter4;
use windows::Win32::Graphics::Dxgi::IDXGIFactory7;
use windows::Win32::Graphics::Dxgi::IDXGIOutput;
use windows::Win32::Graphics::Dxgi::IDXGIOutput6;
use windows::Win32::Graphics::Dxgi::IDXGISwapChain3;
use windows::Win32::Graphics::Dxgi::IDXGISwapChain4;
use windows::core::Interface as _;
use windows::core::PCWSTR;
use zgui::surface::wgpu;
use zgui_render_wgpu::Gpu;

/// The backbuffer count. Two is the minimum for a flip model swapchain; three would
/// give the compositor more slack.
const BUFFERS: u32 = 2;

/// The Vulkan spelling of the 10-bit swapchain format.
const VK_FORMAT: vk::Format = vk::Format::A2B10G10R10_UNORM_PACK32;

/// What to do with HDR.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum HdrMode {
    /// Enable HDR when the display is already in HDR mode.
    #[default]
    Auto,
    /// Require HDR; fail if the display cannot present PQ/Rec.2020.
    On,
    /// Present SDR.
    Off,
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
    /// Kept alive with the swapchain; DXGI holds the association.
    #[expect(dead_code, reason = "D3D12 objects the swapchain is associated with")]
    queue: ID3D12CommandQueue,
    /// Kept alive with the swapchain; the shared handles came from it.
    #[expect(dead_code, reason = "D3D12 objects the swapchain is associated with")]
    device: ID3D12Device,
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
        gpu: &Gpu,
        hwnd: HWND,
        width: u32,
        height: u32,
        hdr: HdrMode,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(width > 0 && height > 0, "zero-sized swapchain");

        // SAFETY: `as_hal` only asks that the resource behind the guard outlives the
        // borrow. Both wgpu handles are alive for the whole call.
        let hal_adapter = unsafe { gpu.adapter().as_hal::<wgpu::hal::api::Vulkan>() }
            .context("the video device is not Vulkan")?;
        // SAFETY: as above.
        let hal_device = unsafe { gpu.device().as_hal::<wgpu::hal::api::Vulkan>() }
            .context("the video device is not Vulkan")?;

        let enabled = hal_device.enabled_device_extensions();
        anyhow::ensure!(
            enabled.contains(&khr::external_memory_win32::NAME),
            "the Vulkan device does not have VK_KHR_external_memory_win32"
        );

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
        let queue: ID3D12CommandQueue = unsafe { device.CreateCommandQueue(&queue_desc) }
            .context("CreateCommandQueue")?;

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
            Scaling: DXGI_SCALING_NONE,
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
            AlphaMode: DXGI_ALPHA_MODE_IGNORE,
            Flags: 0,
        };
        // SAFETY: the queue and HWND are alive, and the descriptor is a valid local.
        let swapchain1 = unsafe {
            factory.CreateSwapChainForHwnd(&queue, hwnd, &desc, None, None::<&IDXGIOutput>)
        }
        .context("CreateSwapChainForHwnd")?;
        let swapchain: IDXGISwapChain3 = swapchain1.cast().context("swapchain vtable")?;
        // SAFETY: the factory and HWND are alive for the call.
        unsafe { factory.MakeWindowAssociation(hwnd, DXGI_MWA_NO_ALT_ENTER) }
            .context("MakeWindowAssociation")?;

        let (hdr, max_luminance, sdr_white) = configure_hdr(&swapchain, hdr)?;
        let format = wgpu::TextureFormat::Rgb10a2Unorm;

        let instance = hal_adapter.shared_instance().raw_instance();
        let mut shared_handles = Vec::with_capacity(BUFFERS as usize);
        let mut backbuffers = Vec::with_capacity(BUFFERS as usize);
        let mut images = Vec::with_capacity(BUFFERS as usize);

        for index in 0..BUFFERS {
            // SAFETY: the swapchain is alive and `index` is below its buffer count.
            let resource: ID3D12Resource = unsafe { swapchain.GetBuffer(index) }
                .with_context(|| format!("GetBuffer({index})"))?;
            let child: ID3D12DeviceChild = resource.cast().context("backbuffer child")?;
            // SAFETY: the resource is a D3D12 texture, the handle is an NT handle, and
            // `GENERIC_ALL` is what the reference DXGI interop path uses.
            let handle = unsafe {
                device.CreateSharedHandle(
                    &child,
                    None,
                    GENERIC_ALL.0,
                    PCWSTR::null(),
                )
            }
            .with_context(|| format!("CreateSharedHandle({index})"))?;

            // SAFETY: the image is created on the same device and memory is bound to it
            // below; `Drop` destroys both exactly once.
            let imported = unsafe {
                import_backbuffer(&hal_device, instance, handle, width, height, VK_FORMAT)
            }
            .with_context(|| format!("import backbuffer {index}"))?;

            images.push(imported);
            shared_handles.push(handle);
            backbuffers.push(resource);
        }

        Ok(Self {
            images,
            shared_handles,
            backbuffers,
            swapchain,
            queue,
            device,
            device_handle: hal_device.raw_device().clone(),
            format,
            width,
            height,
            hdr,
            max_luminance,
            sdr_white,
        })
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
        for (image, memory) in self.images.drain(..) {
            // SAFETY: both came from the device below and are freed once.
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
    anyhow::ensure!(id.device_luid_valid != vk::FALSE, "the Vulkan device has no LUID");
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
fn configure_hdr(
    swapchain: &IDXGISwapChain3,
    mode: HdrMode,
) -> anyhow::Result<(bool, f32, f32)> {
    // SAFETY: the swapchain is alive for the call.
    let output = unsafe { swapchain.GetContainingOutput() }.context("GetContainingOutput")?;
    let output6: IDXGIOutput6 = output.cast().context("IDXGIOutput6")?;
    // SAFETY: `output6` is a live output.
    let desc = unsafe { output6.GetDesc1() }.context("GetDesc1")?;
    // SAFETY: the swapchain is alive for the call.
    let support = unsafe {
        swapchain.CheckColorSpaceSupport(DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020)
    }
    .context("CheckColorSpaceSupport")?;
    let supported = support & (DXGI_SWAP_CHAIN_COLOR_SPACE_SUPPORT_FLAG_PRESENT.0 as u32) != 0;
    eprintln!(
        "DXGI output: color space {:?}, peak {} nits, max full-frame {} nits, bits {}",
        desc.ColorSpace,
        desc.MaxLuminance,
        desc.MaxFullFrameLuminance,
        desc.BitsPerColor
    );

    let active = match mode {
        HdrMode::Off => false,
        HdrMode::On => {
            anyhow::ensure!(supported, "the display does not support PQ/Rec.2020");
            true
        }
        HdrMode::Auto => {
            supported && desc.ColorSpace == DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020
        }
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
    eprintln!(
        "SDR white: {sdr_white:.0} nits (level from the display configuration)"
    );
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
    let memory_properties =
        unsafe { instance.get_physical_device_memory_properties(physical) };
    let candidates = handle_properties.memory_type_bits & requirements.memory_type_bits;
    anyhow::ensure!(candidates != 0, "no Vulkan memory type for the D3D12 resource");
    let memory_type = (0..memory_properties.memory_type_count)
        .find(|index| {
            candidates & (1 << index) != 0
                && memory_properties.memory_types[*index as usize]
                    .property_flags
                    .contains(vk::MemoryPropertyFlags::DEVICE_LOCAL)
        })
        .or_else(|| (0..memory_properties.memory_type_count).find(|index| candidates & (1 << index) != 0))
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
