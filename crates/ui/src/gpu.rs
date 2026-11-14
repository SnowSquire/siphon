//! The one Vulkan device the UI, libplacebo and FFmpeg share.
//!
//! wgpu creates it, because wgpu cannot adopt an externally created device.
//! The hal callback adds what wgpu never asks for:
//!
//! * the Vulkan 1.2 features libplacebo requires (`hostQueryReset`,
//!   `timelineSemaphore`);
//! * the video-decode extensions and a queue in the video-decode family, which
//!   FFmpeg's Vulkan decoder submits on (NVIDIA keeps that family separate from
//!   the graphics one, so wgpu would never create a queue there).

// Ported from the video spike: the video-decode queue is not used by the UI.
#![allow(dead_code)]
use std::ffi::CStr;
use std::ffi::c_void;
use std::ptr;
use std::sync::Arc;

use anyhow::Context as _;
use ash::vk;
use zgui::surface::wgpu;
use zgui_render_wgpu::Gpu;

/// `VK_QUEUE_VIDEO_DECODE_BIT_KHR`; ash has no binding for the video enums.
const QUEUE_VIDEO_DECODE: vk::QueueFlags = vk::QueueFlags::from_raw(0x20);

/// `VK_STRUCTURE_TYPE_QUEUE_FAMILY_VIDEO_PROPERTIES_KHR`.
const QUEUE_FAMILY_VIDEO_PROPERTIES: i32 = 1_000_023_012;

/// `VK_VIDEO_CODEC_OPERATION_DECODE_H265_BIT_KHR`.
const DECODE_H265: u32 = 0x2;

/// `VkQueueFamilyVideoPropertiesKHR`, absent from ash.
#[repr(C)]
struct QueueFamilyVideoProperties {
    s_type: i32,
    p_next: *mut c_void,
    video_codec_operations: u32,
}

/// The device extensions FFmpeg's Vulkan decoder loads functions from. The two
/// queue ones are required; the codec ones are enabled when the driver has them.
const VIDEO_EXTENSIONS: [&CStr; 6] = [
    c"VK_KHR_video_queue",
    c"VK_KHR_video_decode_queue",
    c"VK_KHR_video_decode_h264",
    c"VK_KHR_video_decode_h265",
    c"VK_KHR_video_decode_vp9",
    c"VK_KHR_video_decode_av1",
];

/// One-priority queue creation, which `DeviceQueueCreateInfo` borrows.
static PRIORITIES: [f32; 1] = [1.0];

/// A queue family that decodes HEVC, with the codec operations the driver
/// reports for it. FFmpeg wants both in its device context.
#[derive(Clone, Copy)]
pub struct VideoDecode {
    pub family: u32,
    pub operations: u32,
}

/// The device plus the video-decode queue FFmpeg must submit on, if the driver
/// has Vulkan video decode at all.
pub fn create() -> anyhow::Result<(Arc<Gpu>, Option<VideoDecode>)> {
    let mut instance_descriptor = wgpu::InstanceDescriptor::new_without_display_handle();
    instance_descriptor.backends = wgpu::Backends::VULKAN;
    let instance = wgpu::Instance::new(instance_descriptor);
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        compatible_surface: None,
        force_fallback_adapter: false,
    }))
    .context("no Vulkan adapter")?;

    let descriptor = wgpu::DeviceDescriptor {
        label: Some("video.device"),
        required_features: wgpu::Features::empty(),
        required_limits: adapter.limits(),
        memory_hints: wgpu::MemoryHints::default(),
        ..Default::default()
    };

    let (device, queue, decode) = {
        // SAFETY: `as_hal` requires the resource behind the guard not to be
        // destroyed while borrowed. Nothing here destroys it.
        let hal_adapter = unsafe { adapter.as_hal::<wgpu::hal::api::Vulkan>() }
            .context("the adapter is not Vulkan")?;
        let (decode, extensions) = video_support(&hal_adapter);

        let callback: Box<wgpu::hal::vulkan::CreateDeviceCallback<'_>> = Box::new(move |args| {
            // The p_next chain points into this after the callback returns,
            // so it has to outlive the device creation call. One tiny leak
            // per device is cheaper than a vendored wgpu-hal.
            let features: &'static mut vk::PhysicalDeviceVulkan12Features<'static> =
                Box::leak(Box::new(
                    vk::PhysicalDeviceVulkan12Features::default()
                        .host_query_reset(true)
                        .timeline_semaphore(true),
                ));
            let info = std::mem::take(args.create_info);
            *args.create_info = info.push_next(features);

            args.extensions.extend_from_slice(&extensions);
            if let Some(decode) = decode
                && !args
                    .queue_create_infos
                    .iter()
                    .any(|info| info.queue_family_index == decode.family)
            {
                args.queue_create_infos.push(
                    vk::DeviceQueueCreateInfo::default()
                        .queue_family_index(decode.family)
                        .queue_priorities(&PRIORITIES),
                );
            }
        });

        // SAFETY: the callback only enables features and extensions the
        // physical device advertises, and leaves every other create-info field
        // as wgpu built it.
        let opened = unsafe {
            hal_adapter.open_with_callback(
                descriptor.required_features,
                &descriptor.required_limits,
                &descriptor.memory_hints,
                Some(callback),
            )
        }
        .context("open Vulkan device with libplacebo and video decode")?;
        // SAFETY: `opened` came from this adapter immediately above, and the
        // descriptor states the features and limits it was created with.
        let (device, queue) = unsafe { adapter.create_device_from_hal(opened, &descriptor) }
            .context("adopt the hal device")?;
        (device, queue, decode)
    };

    Ok((Gpu::from_existing(instance, adapter, device, queue), decode))
}

/// Finds the queue family that decodes HEVC and the decode extensions the
/// driver has.
///
/// Returns `None` when the driver cannot decode video at all, in which case the
/// decoder falls back to whatever else it finds.
fn video_support(
    adapter: &wgpu::hal::vulkan::Adapter,
) -> (Option<VideoDecode>, Vec<&'static CStr>) {
    let instance = adapter.shared_instance().raw_instance();
    let physical = adapter.raw_physical_device();

    // SAFETY: `physical` belongs to `instance`, and both outlive the calls.
    let advertised = unsafe { instance.enumerate_device_extension_properties(physical) };
    let Ok(advertised) = advertised else {
        return (None, Vec::new());
    };
    let has = |name: &CStr| {
        advertised.iter().any(|extension| {
            // SAFETY: `extension_name` is a NUL-terminated 256-byte array.
            (unsafe { CStr::from_ptr(extension.extension_name.as_ptr()) }) == name
        })
    };
    if !has(VIDEO_EXTENSIONS[0]) || !has(VIDEO_EXTENSIONS[1]) {
        return (None, Vec::new());
    }

    // SAFETY: `physical` belongs to `instance`; the returned properties are a
    // snapshot that owns no handles.
    let count = unsafe { instance.get_physical_device_queue_family_properties2_len(physical) };
    let mut video: Vec<QueueFamilyVideoProperties> = (0..count)
        .map(|_| QueueFamilyVideoProperties {
            s_type: QUEUE_FAMILY_VIDEO_PROPERTIES,
            p_next: ptr::null_mut(),
            video_codec_operations: 0,
        })
        .collect();
    let mut families = vec![vk::QueueFamilyProperties2::default(); count];
    for (family, video) in families.iter_mut().zip(&mut video) {
        family.p_next = ptr::from_mut(video).cast();
    }
    // SAFETY: every `p_next` points at a live element of `video`, and each
    // element of `families` is written once.
    unsafe { instance.get_physical_device_queue_family_properties2(physical, &mut families) };

    let decode =
        families
            .iter()
            .zip(&video)
            .enumerate()
            .find_map(|(family, (properties, video))| {
                (properties
                    .queue_family_properties
                    .queue_flags
                    .contains(QUEUE_VIDEO_DECODE)
                    && video.video_codec_operations & DECODE_H265 != 0)
                    .then(|| VideoDecode {
                        family: u32::try_from(family).unwrap_or(0),
                        operations: video.video_codec_operations,
                    })
            });
    if decode.is_none() {
        return (None, Vec::new());
    }

    let extensions = VIDEO_EXTENSIONS
        .into_iter()
        .filter(|name| has(name))
        .collect();
    (decode, extensions)
}
