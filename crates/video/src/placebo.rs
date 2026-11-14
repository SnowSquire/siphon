//! libplacebo on the wgpu Vulkan device.
//!
//! The device is imported, not created: wgpu owns it and libplacebo shares the
//! instance, physical device, device and graphics queue.

use std::ffi::c_char;
use std::ptr;

use anyhow::Context as _;
use ash::vk;
use ash::vk::Handle as _;
use libplacebo_sys as pl;
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

        let log_params = pl::pl_log_params {
            log_cb: Some(pl::pl_log_simple),
            log_level: pl::pl_log_level::PL_LOG_INFO,
            ..Default::default()
        };
        // SAFETY: the parameters are a live local and libplacebo returns an
        // owned log or null.
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
        let features = pl::VkPhysicalDeviceFeatures2 {
            sType: pl::VkStructureType::VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_FEATURES_2,
            pNext: (&raw mut vk12).cast(),
            ..Default::default()
        };

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
            // SAFETY: the log was created just above and is destroyed once.
            unsafe { pl::pl_log_destroy(&raw const log as *mut _) };
            anyhow::bail!("libplacebo refused to import the Vulkan device");
        }

        // SAFETY: a successfully imported `pl_vulkan` owns a `pl_gpu`.
        let gpu = unsafe { (*vulkan).gpu };
        // SAFETY: the log and GPU are live; libplacebo returns a renderer or null.
        let renderer = unsafe { pl::pl_renderer_create(log, gpu) };
        if renderer.is_null() {
            // SAFETY: both were created above, are owned here, and are
            // destroyed once, in dependency order.
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

    /// Wraps a `VkImage` someone else owns into a libplacebo texture.
    ///
    /// The image must have been created with `usage` and stay alive until
    /// [`Placebo::destroy_texture`] has run on the returned texture.
    pub fn wrap(
        &self,
        image: vk::Image,
        width: u32,
        height: u32,
        format: vk::Format,
        usage: vk::ImageUsageFlags,
    ) -> anyhow::Result<pl::pl_tex> {
        let params = pl::pl_vulkan_wrap_params {
            image: image.as_raw() as usize as *mut pl::VkImage_T,
            aspect: vk::ImageAspectFlags::COLOR.as_raw(),
            width: std::os::raw::c_int::try_from(width).unwrap_or(0),
            height: std::os::raw::c_int::try_from(height).unwrap_or(0),
            // Zero, not one: libplacebo spells "2D" as a missing depth.
            depth: 0,
            format: vk_format(format),
            usage: usage.as_raw(),
            ..Default::default()
        };
        // SAFETY: the image lives on this GPU's device and matches the description.
        let texture = unsafe { pl::pl_vulkan_wrap(self.gpu, &params) };
        anyhow::ensure!(!texture.is_null(), "libplacebo refused to wrap the image");
        Ok(texture)
    }

    /// Destroys a texture from [`Placebo::wrap`] (or `pl_tex_create`).
    pub fn destroy_texture(&self, texture: &mut pl::pl_tex) {
        // SAFETY: the texture came from this GPU and is destroyed once.
        unsafe { pl::pl_tex_destroy(self.gpu, texture) };
    }

    /// Hands a wrapped image back to libplacebo, saying what layout it is in.
    ///
    /// Wrapped images start held by their owner; every `pl_tex` call before a
    /// release is undefined behaviour.
    pub fn release(&self, texture: pl::pl_tex, layout: pl::VkImageLayout) {
        let params = pl::pl_vulkan_release_params {
            tex: texture,
            layout,
            qf: u32::MAX,
            ..Default::default()
        };
        // SAFETY: the texture is held by us and the layout describes the image.
        unsafe { pl::pl_vulkan_release_ex(self.gpu, &params) };
    }

    /// Takes a wrapped image back, transitioning it to `layout` on the way.
    pub fn hold(&self, texture: pl::pl_tex, layout: pl::VkImageLayout, semaphore: pl::pl_vulkan_sem) -> bool {
        let params = pl::pl_vulkan_hold_params {
            tex: texture,
            layout,
            qf: u32::MAX,
            semaphore,
            ..Default::default()
        };
        // SAFETY: libplacebo owns the texture and finishes with it before the
        // semaphore fires.
        unsafe { pl::pl_vulkan_hold_ex(self.gpu, &params) }
    }

    /// A timeline semaphore for the wrap/hold handovers.
    pub fn create_semaphore(&self) -> pl::VkSemaphore {
        let params = pl::pl_vulkan_sem_params {
            type_: pl::VkSemaphoreType::VK_SEMAPHORE_TYPE_TIMELINE,
            initial_value: 0,
            ..Default::default()
        };
        // SAFETY: the GPU is live; the returned handle is destroyed with
        // [`Placebo::destroy_semaphore`].
        unsafe { pl::pl_vulkan_sem_create(self.gpu, &params) }
    }

    /// Destroys a semaphore from [`Placebo::create_semaphore`].
    pub fn destroy_semaphore(&self, semaphore: &mut pl::VkSemaphore) {
        // SAFETY: the semaphore came from this GPU and is destroyed once.
        unsafe { pl::pl_vulkan_sem_destroy(self.gpu, semaphore) };
    }

    /// Wraps a wgpu texture that was created `Bgra8Unorm` with
    /// `RENDER_ATTACHMENT | TEXTURE_BINDING`, which is what ZGUI draws into.
    pub fn wrap_wgpu(&self, texture: &wgpu::Texture) -> anyhow::Result<pl::pl_tex> {
        anyhow::ensure!(
            texture.format() == wgpu::TextureFormat::Bgra8Unorm,
            "the interface texture is not Bgra8Unorm"
        );
        // SAFETY: `as_hal` only asks that the texture outlives the borrow.
        let hal = unsafe { texture.as_hal::<wgpu::hal::api::Vulkan>() }
            .context("the interface texture is not Vulkan")?;
        // SAFETY: the texture is alive and owned by the caller, who keeps it
        // until the returned texture is destroyed.
        let image = unsafe { hal.raw_handle() };
        let size = texture.size();
        self.wrap(
            image,
            size.width,
            size.height,
            vk::Format::B8G8R8A8_UNORM,
            vk::ImageUsageFlags::COLOR_ATTACHMENT | vk::ImageUsageFlags::SAMPLED,
        )
    }

    /// Renders `image` (or just the target's overlays) into `target`.
    pub fn render(&self, image: Option<&pl::pl_frame>, target: &pl::pl_frame) -> bool {
        let params = pl::pl_render_params::default();
        // SAFETY: both frames describe live textures on this GPU, and the render
        // params are libplacebo's own defaults (null pointers, black background).
        unsafe {
            pl::pl_render_image(
                self.renderer,
                image.map_or(ptr::null(), ptr::from_ref),
                target,
                &params,
            )
        }
    }

    /// Blocks until every submission on the GPU has completed.
    pub fn finish(&self) {
        // SAFETY: the GPU is live for the borrow.
        unsafe { pl::pl_gpu_finish(self.gpu) };
    }
}

/// Converts an ash Vulkan format into libplacebo's spelling of the same enum.
///
/// bindgen emits `VkFormat` as a Rust enum; both sides carry the same
/// discriminant, and `layout_tests` is off, so the conversion is by value.
fn vk_format(format: vk::Format) -> pl::VkFormat {
    // SAFETY: `ash::vk::Format` is the same Vulkan enum; every value used here
    // is a valid `VkFormat` variant.
    unsafe { std::mem::transmute::<i32, pl::VkFormat>(format.as_raw() as i32) }
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


