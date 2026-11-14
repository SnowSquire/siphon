//! libplacebo on the wgpu Vulkan device.
//!
//! The device is imported, not created: wgpu owns it and libplacebo shares the
//! instance, physical device, device and graphics queue.

use std::ffi::c_char;
use std::ptr;

use anyhow::Context as _;
use ash::vk;
use ash::vk::Handle as _;
use ffcine_libplacebo_sys as pl;
use zgui::surface::wgpu;
use zgui_render_wgpu::Gpu;

pub struct Placebo {
    log: pl::pl_log,
    vulkan: pl::pl_vulkan,
    gpu: pl::pl_gpu,
    renderer: pl::pl_renderer,
    /// Keeps the Vulkan loader alive for the `get_proc_addr` pointer below.
    _entry: ash::Entry,
}

impl Placebo {
    pub fn import(gpu: &Gpu) -> anyhow::Result<Self> {
        // SAFETY: `as_hal` only asks that the resource behind the guard is not
        // destroyed while borrowed. Both wgpu handles live for the call.
        let hal_adapter = unsafe { gpu.adapter().as_hal::<wgpu::hal::api::Vulkan>() }
            .context("the video device is not Vulkan")?;
        // SAFETY: as above.
        let hal_device = unsafe { gpu.device().as_hal::<wgpu::hal::api::Vulkan>() }
            .context("the video device is not Vulkan")?;

        let mut log_params = pl::pl_log_params::default();
        log_params.log_cb = Some(pl::pl_log_simple);
        log_params.log_level = pl::pl_log_level::PL_LOG_INFO;
        let log = unsafe {
            pl::pl_log_create_372(pl::PL_API_VER as std::os::raw::c_int, &log_params)
        };
        anyhow::ensure!(!log.is_null(), "libplacebo refused to create a log");

        let instance = hal_adapter.shared_instance().raw_instance();
        // SAFETY: loading the system Vulkan loader; `Placebo` owns the entry for
        // as long as any pointer derived from it is used.
        let entry = unsafe { ash::Entry::load() }.context("load vulkan-1.dll")?;
        // SAFETY: both are the same Vulkan entry point, differing only in the
        // bindings' spelling of the handle types. The ABI and size are identical.
        let get_proc_addr: pl::PFN_vkGetInstanceProcAddr =
            unsafe { std::mem::transmute(entry.static_fn().get_instance_proc_addr) };
        let extensions: Vec<*const c_char> = hal_device
            .enabled_device_extensions()
            .iter()
            .map(|name| name.as_ptr())
            .collect();

        // libplacebo's required features, built here instead of referencing its
        // exported data symbol, which the import library does not alias directly.
        let mut vk12 = vk::PhysicalDeviceVulkan12Features::default()
            .host_query_reset(true)
            .timeline_semaphore(true);
        let mut features = pl::VkPhysicalDeviceFeatures2::default();
        features.sType = pl::VkStructureType::VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_FEATURES_2;
        features.pNext = (&raw mut vk12).cast();

        let params = pl::pl_vulkan_import_params {
            instance: instance.handle().as_raw() as usize as *mut pl::VkInstance_T,
            get_proc_addr,
            phys_device: hal_device.raw_physical_device().as_raw() as usize
                as *mut pl::VkPhysicalDevice_T,
            device: hal_device.raw_device().handle().as_raw() as usize
                as *mut pl::VkDevice_T,
            extensions: extensions.as_ptr(),
            num_extensions: std::os::raw::c_int::try_from(extensions.len()).unwrap_or(0),
            queue_graphics: pl::pl_vulkan_queue {
                index: hal_device.queue_family_index(),
                count: 1,
                flags: 0,
            },
            // No dedicated compute or transfer queues: libplacebo falls back to
            // the graphics queue.
            queue_compute: pl::pl_vulkan_queue::default(),
            queue_transfer: pl::pl_vulkan_queue::default(),
            features: &raw const features,
            lock_queue: None,
            unlock_queue: None,
            queue_ctx: ptr::null_mut(),
            no_compute: true,
            max_glsl_version: 0,
            max_api_version: 0,
        };

        // SAFETY: every handle and extension pointer above belongs to the live
        // wgpu device, and the required-features structure is libplacebo's own.
        let vulkan = unsafe { pl::pl_vulkan_import(log, &params) };
        if vulkan.is_null() {
            unsafe { pl::pl_log_destroy(&raw const log as *mut _) };
            anyhow::bail!("libplacebo refused to import the Vulkan device");
        }

        // SAFETY: a successfully imported `pl_vulkan` owns a `pl_gpu`.
        let gpu = unsafe { (*vulkan).gpu };
        // SAFETY: the log and GPU are live; libplacebo returns a renderer or null.
        let renderer = unsafe { pl::pl_renderer_create(log, gpu) };
        if renderer.is_null() {
            unsafe {
                pl::pl_vulkan_destroy(&raw const vulkan as *mut _);
                pl::pl_log_destroy(&raw const log as *mut _);
            }
            anyhow::bail!("libplacebo refused to create a renderer");
        }

        Ok(Self {
            log,
            vulkan,
            gpu,
            renderer,
            _entry: entry,
        })
    }

    pub const fn gpu(&self) -> pl::pl_gpu {
        self.gpu
    }

    pub const fn renderer(&self) -> pl::pl_renderer {
        self.renderer
    }
}

impl Drop for Placebo {
    fn drop(&mut self) {
        // SAFETY: each pointer came from the matching constructor and is
        // destroyed once; dependents are destroyed before their dependencies.
        unsafe {
            pl::pl_renderer_destroy(&raw mut self.renderer);
            pl::pl_vulkan_destroy(&raw mut self.vulkan);
            pl::pl_log_destroy(&raw mut self.log);
        }
    }
}


