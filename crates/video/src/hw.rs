//! FFmpeg's Vulkan hardware device, built on the device wgpu already owns.
//!
//! The decoder is given this device instead of creating one of its own, so its
//! output frames are VkImages on the same device libplacebo renders with.

use std::ffi::c_char;
use std::ffi::c_void;
use std::ptr;

use std::path::Path;

use anyhow::Context as _;
use anyhow::bail;
use ash::vk;
use ash::vk::Handle as _;
use ffmpeg_the_third::codec;
use ffmpeg_the_third::ffi;
use ffmpeg_the_third::format;
use ffmpeg_the_third::frame;
use ffmpeg_the_third::media::Type;
use zgui::surface::wgpu;
use zgui_render_wgpu::Gpu;

use crate::gpu::VideoDecode;

/// `AV_PIX_FMT_VULKAN` in FFmpeg 9.
pub const AV_PIX_FMT_VULKAN: ffi::AVPixelFormat = ffi::AVPixelFormat(190);

#[repr(C)]
#[derive(Clone, Copy)]
struct QueueFamily {
    idx: std::os::raw::c_int,
    num: std::os::raw::c_int,
    flags: std::os::raw::c_int,
    video_caps: std::os::raw::c_int,
}

/// `VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_FEATURES_2`.
const PHYSICAL_DEVICE_FEATURES_2: i32 = 1_000_059_000;

/// `VkPhysicalDeviceFeatures2`, which the FFmpeg bindings do not define.
#[repr(C)]
struct PhysicalDeviceFeatures2 {
    s_type: i32,
    p_next: *mut c_void,
    features: vk::PhysicalDeviceFeatures,
}

#[repr(C)]
struct VulkanDeviceContext {
    alloc: *const c_void,
    get_proc_addr: Option<unsafe extern "system" fn(*mut c_void, *const c_char) -> *const c_void>,
    inst: *mut c_void,
    phys_dev: *mut c_void,
    act_dev: *mut c_void,
    device_features: PhysicalDeviceFeatures2,
    enabled_inst_extensions: *const *const c_char,
    nb_enabled_inst_extensions: std::os::raw::c_int,
    enabled_dev_extensions: *const *const c_char,
    nb_enabled_dev_extensions: std::os::raw::c_int,
    lock_queue: Option<unsafe extern "C" fn(*mut c_void, u32, u32)>,
    unlock_queue: Option<unsafe extern "C" fn(*mut c_void, u32, u32)>,
    qf: [QueueFamily; 64],
    nb_qf: std::os::raw::c_int,
    queue_flags: u32,
}

pub struct HwDevice {
    device_ref: *mut ffi::AVBufferRef,
    /// Keeps the Vulkan loader behind `get_proc_addr` alive.
    _entry: ash::Entry,
    /// Keeps the extension-name pointers in the context valid.
    _extensions: Vec<*const c_char>,
}

// SAFETY: `HwDevice` is only used from the decoder thread and the UI thread in
// turn; FFmpeg's Vulkan frames carry their own semaphores. This is a raw handle
// owner, not a concurrent container.
unsafe impl Send for HwDevice {}

impl HwDevice {
    /// `video` is the queue family `crate::gpu::create` enabled for decoding.
    pub fn new(gpu: &Gpu, video: VideoDecode) -> anyhow::Result<Self> {
        // SAFETY: the hal guards are only read; the wgpu handles outlive them.
        let hal_adapter = unsafe { gpu.adapter().as_hal::<wgpu::hal::api::Vulkan>() }
            .context("the video device is not Vulkan")?;
        // SAFETY: as above.
        let hal_device = unsafe { gpu.device().as_hal::<wgpu::hal::api::Vulkan>() }
            .context("the video device is not Vulkan")?;

        let raw_instance = hal_adapter.shared_instance().raw_instance();
        let phys_device = hal_device.raw_physical_device();
        // SAFETY: the physical device belongs to the instance, and both outlive
        // the query.
        let families = unsafe { raw_instance.get_physical_device_queue_family_properties(phys_device) };
        let family = |index: u32| -> anyhow::Result<QueueFamily> {
            let properties = families
                .get(index as usize)
                .with_context(|| format!("queue family {index} does not exist"))?;
            Ok(QueueFamily {
                idx: std::os::raw::c_int::try_from(index).unwrap_or(0),
                num: 1,
                flags: properties.queue_flags.as_raw() as std::os::raw::c_int,
                // Filled in by FFmpeg from the driver's codec capabilities.
                video_caps: 0,
            })
        };
        let graphics = family(hal_device.queue_family_index())?;
        let mut decode = family(video.family)?;
        decode.video_caps = std::os::raw::c_int::try_from(video.operations).unwrap_or(0);

        // SAFETY: loading the system Vulkan loader; the entry is kept in `Self`.
        let entry = unsafe { ash::Entry::load() }.context("load vulkan-1.dll")?;
        // SAFETY: both are the same Vulkan entry point under different bindings.
        let get_proc_addr: Option<
            unsafe extern "system" fn(*mut c_void, *const c_char) -> *const c_void,
        > = unsafe { std::mem::transmute(entry.static_fn().get_instance_proc_addr) };

        let extensions: Vec<*const c_char> = hal_device
            .enabled_device_extensions()
            .iter()
            .map(|name| name.as_ptr())
            .collect();

        // SAFETY: allocates an AVHWDeviceContext of the Vulkan type. The
        // returned reference is freed in Drop.
        let device_ref = unsafe { ffi::av_hwdevice_ctx_alloc(ffi::AVHWDeviceType::VULKAN) };
        anyhow::ensure!(!device_ref.is_null(), "av_hwdevice_ctx_alloc failed");

        // SAFETY: the reference was just allocated and points at an
        // AVHWDeviceContext.
        unsafe {
            let ctx = (*device_ref).data.cast::<ffi::AVHWDeviceContext>();
            let vk = (*ctx).hwctx.cast::<VulkanDeviceContext>();
            ptr::write_bytes(vk, 0, 1);

            (*vk).get_proc_addr = get_proc_addr;
            (*vk).inst = hal_adapter
                .shared_instance()
                .raw_instance()
                .handle()
                .as_raw() as usize as *mut c_void;
            (*vk).phys_dev = hal_device.raw_physical_device().as_raw() as usize as *mut c_void;
            (*vk).act_dev =
                hal_device.raw_device().handle().as_raw() as usize as *mut c_void;
            (*vk).enabled_dev_extensions = extensions.as_ptr();
            (*vk).nb_enabled_dev_extensions =
                std::os::raw::c_int::try_from(extensions.len()).unwrap_or(0);
            (*vk).qf[0] = graphics;
            (*vk).qf[1] = decode;
            (*vk).nb_qf = 2;

            // The four features FFmpeg's Vulkan hwcontext expects, plus the two
            // libplacebo needs. The device was created with all of them.
            let mut vk12 = vk::PhysicalDeviceVulkan12Features::default()
                .host_query_reset(true)
                .timeline_semaphore(true);
            let features = vk::PhysicalDeviceFeatures {
                shader_image_gather_extended: vk::TRUE,
                fragment_stores_and_atomics: vk::TRUE,
                shader_int64: vk::TRUE,
                vertex_pipeline_stores_and_atomics: vk::TRUE,
                ..Default::default()
            };
            (*vk).device_features = PhysicalDeviceFeatures2 {
                s_type: PHYSICAL_DEVICE_FEATURES_2,
                p_next: (&raw mut vk12).cast(),
                features,
            };

            // Keep the device alive: FFmpeg must not destroy what wgpu owns.
            (*ctx).free = Some(ignore_free);
        }

        // SAFETY: the context is fully populated and owned by `device_ref`.
        let result = unsafe { ffi::av_hwdevice_ctx_init(device_ref) };
        if result < 0 {
            let mut failed = device_ref;
            // SAFETY: the reference is ours and not stored yet.
            unsafe { ffi::av_buffer_unref(&raw mut failed) };
            anyhow::bail!("av_hwdevice_ctx_init failed: {}", av_error(result));
        }

        Ok(Self {
            device_ref,
            _entry: entry,
            _extensions: extensions,
        })
    }

    #[must_use]
    pub const fn as_ptr(&self) -> *mut ffi::AVBufferRef {
        self.device_ref
    }
}

impl Drop for HwDevice {
    fn drop(&mut self) {
        // SAFETY: `device_ref` is a live reference from av_hwdevice_ctx_alloc.
        unsafe { ffi::av_buffer_unref(&raw mut self.device_ref) };
    }
}

const unsafe extern "C" fn ignore_free(_ctx: *mut ffi::AVHWDeviceContext) {}

fn av_error(code: i32) -> String {
    let mut buffer = [0i8; 256];
    // SAFETY: `buffer` is writable for 256 bytes; the call writes a C string.
    unsafe {
        ffi::av_strerror(code, buffer.as_mut_ptr(), buffer.len());
        std::ffi::CStr::from_ptr(buffer.as_ptr())
            .to_string_lossy()
            .into_owned()
    }
}

/// Decodes one frame through the Vulkan hardware path and reports its format.
pub fn probe(gpu: &Gpu, path: &Path, video: VideoDecode) -> anyhow::Result<()> {
    let hardware = HwDevice::new(gpu, video)?;
    let mut input = format::input(path).with_context(|| format!("open {}", path.display()))?;
    let (stream_index, mut context) = {
        let stream = input
            .streams()
            .best(Type::Video)
            .context("no video stream")?;
        (
            stream.index(),
            codec::context::Context::from_parameters(stream.parameters())?,
        )
    };

    // SAFETY: the context is not open yet; the fields are set before
    // `decoder().video()` opens it. The device reference is retained by FFmpeg.
    unsafe {
        let raw = context.as_mut_ptr();
        (*raw).hw_device_ctx = ffi::av_buffer_ref(hardware.as_ptr());
        (*raw).get_format = Some(choose_vulkan_format);
        (*raw).thread_count = 1;
    }
    let mut decoder = context.decoder().video()?;

    let mut picture = frame::Video::empty();
    for packet in input.packets() {
        let (stream, packet) = packet?;
        if stream.index() != stream_index {
            continue;
        }
        decoder.send_packet(&packet)?;
        if decoder.receive_frame(&mut picture).is_ok() {
            // SAFETY: the frame is live for the borrow.
            let format = unsafe { (*picture.as_ptr()).format };
            eprintln!(
                "hw probe: frame format {format}, {}x{}",
                picture.width(),
                picture.height()
            );
            return Ok(());
        }
    }
    bail!("no frame decoded")
}

/// Tells FFmpeg to use Vulkan for this decoder.
///
/// # Safety
///
/// `fmt` must be a valid, `AV_PIX_FMT_NONE`-terminated format list.
pub const unsafe extern "C" fn choose_vulkan_format(
    _ctx: *mut ffi::AVCodecContext,
    fmt: *const ffi::AVPixelFormat,
) -> ffi::AVPixelFormat {
    let mut current = fmt;
    // SAFETY: the caller passes FFmpeg's terminated format list.
    unsafe {
        while !current.is_null() && (*current).0 != -1 {
            if (*current).0 == AV_PIX_FMT_VULKAN.0 {
                return AV_PIX_FMT_VULKAN;
            }
            current = current.add(1);
        }
    }
    ffi::AVPixelFormat(-1)
}
