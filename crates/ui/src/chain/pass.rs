//! The presentation pass: the interface drawn over the diagnostics fill,
//! straight into the DXGI backbuffer.
//!
//! There is no intermediate image and no blit. The pass clears the whole canvas
//! to magenta and draws one triangle that samples the interface texture with
//! premultiplied alpha, so the fill shows wherever the interface is not fully
//! opaque — including where ZGUI never wrote anything. Magenta is therefore a
//! sample that came back short, which is the band being chased.
//!
//! A compute pass then counts the fill colour in the finished frame and writes
//! the tally into a host-visible buffer that stays mapped. The CPU reads it
//! whenever it likes: no fence, no copy, no stall.

use std::cell::Cell;
use std::ptr;

use ash::vk;
use ash::vk::Handle as _;
use eros::Context as _;
use eros::IntoUnion as _;
use zgui_render_wgpu::Gpu;
use zgui_render_wgpu::wgpu;

/// The fill. The interface never produces this colour, so a texel of it is
/// unambiguously a frame that was not covered.
const FILL: [f32; 4] = [1.0, 0.0, 1.0, 1.0];

/// Straight alpha, sRGB-encoded, as ZGUI leaves it.
const UI_FORMAT: vk::Format = vk::Format::B8G8R8A8_UNORM;

/// One backbuffer's view and framebuffer.
struct Target {
    image: vk::Image,
    view: vk::ImageView,
    framebuffer: vk::Framebuffer,
    scan_set: vk::DescriptorSet,
}

pub struct Pass {
    device: ash::Device,
    render: vk::RenderPass,
    pipeline_layout: vk::PipelineLayout,
    pipeline: vk::Pipeline,
    scan_layout: vk::PipelineLayout,
    scan_pipeline: vk::Pipeline,
    sampler: vk::Sampler,
    ui_layout: vk::DescriptorSetLayout,
    scan_set_layout: vk::DescriptorSetLayout,
    pool: vk::DescriptorPool,
    targets: Vec<Target>,
    /// The interface texture's view and the set that binds it, replaced when
    /// ZGUI hands over a new texture.
    ui_view: vk::ImageView,
    ui_set: vk::DescriptorSet,
    ui_size: (u32, u32),
    format: vk::Format,
    /// The tally the scan writes, mapped for the life of the pass.
    report_buffer: vk::Buffer,
    report_memory: vk::DeviceMemory,
    report: *mut u32,
    frame: Cell<u64>,
    reported: Cell<u64>,
}

impl Pass {
    /// Builds the pipelines for a target of `format`.
    pub fn new(gpu: &Gpu, format: vk::Format) -> eros::Result<Self> {
        // SAFETY: `as_hal` only asks that the resource behind the guard outlives
        // the borrow. Both wgpu handles are alive for the whole call.
        let hal_adapter = unsafe { gpu.adapter().as_hal::<wgpu::hal::api::Vulkan>() }
            .context("the video device is not Vulkan")?;
        // SAFETY: as above.
        let hal_device = unsafe { gpu.device().as_hal::<wgpu::hal::api::Vulkan>() }
            .context("the video device is not Vulkan")?;
        let instance = hal_adapter.shared_instance().raw_instance();
        // SAFETY: the handle is the device wgpu created from this instance;
        // loading its entry points again only reads them.
        let device =
            unsafe { ash::Device::load(instance.fp_v1_0(), hal_device.raw_device().handle()) };

        let ui_layout = descriptor_layout(
            &device,
            &[(
                0,
                vk::DescriptorType::COMBINED_IMAGE_SAMPLER,
                vk::ShaderStageFlags::FRAGMENT,
            )],
        )?;
        let scan_set_layout = descriptor_layout(
            &device,
            &[
                (
                    0,
                    vk::DescriptorType::COMBINED_IMAGE_SAMPLER,
                    vk::ShaderStageFlags::COMPUTE,
                ),
                (
                    1,
                    vk::DescriptorType::STORAGE_BUFFER,
                    vk::ShaderStageFlags::COMPUTE,
                ),
            ],
        )?;
        let pipeline_layout = make_pipeline_layout(&device, ui_layout, vk::ShaderStageFlags::FRAGMENT)?;
        let scan_layout = make_pipeline_layout(&device, scan_set_layout, vk::ShaderStageFlags::COMPUTE)?;

        let pass = render_pass(&device, format)?;
        let pipeline = graphics_pipeline(&device, pass, pipeline_layout)?;
        let scan_pipeline = compute_pipeline(&device, scan_layout)?;
        let sampler = sampler(&device)?;

        let pool_sizes = [
            vk::DescriptorPoolSize {
                ty: vk::DescriptorType::COMBINED_IMAGE_SAMPLER,
                descriptor_count: 8,
            },
            vk::DescriptorPoolSize {
                ty: vk::DescriptorType::STORAGE_BUFFER,
                descriptor_count: 8,
            },
        ];
        // SAFETY: the device is live and the pool sizes are locals.
        let pool = unsafe {
            device.create_descriptor_pool(
                &vk::DescriptorPoolCreateInfo::default()
                    .max_sets(16)
                    .pool_sizes(&pool_sizes),
                None,
            )
        }
        .context("vkCreateDescriptorPool")?;

        let (report_buffer, report_memory, report) = report_buffer(&device, &hal_device)?;

        Ok(Self {
            device,
            render: pass,
            pipeline_layout,
            pipeline,
            scan_layout,
            scan_pipeline,
            sampler,
            ui_layout,
            scan_set_layout,
            pool,
            targets: Vec::with_capacity(crate::dxgi::BUFFERS as usize),
            ui_view: vk::ImageView::null(),
            ui_set: vk::DescriptorSet::null(),
            ui_size: (0, 0),
            format,
            report_buffer,
            report_memory,
            report,
            frame: Cell::new(0),
            reported: Cell::new(0),
        })
    }

    /// Points the pass at the interface texture ZGUI composes into.
    pub fn set_ui(&mut self, texture: &wgpu::Texture) -> eros::Result<()> {
        // Read the size before taking the hal guard: wgpu's own accessors take
        // the same snatch lock and would recurse.
        let size = (texture.width(), texture.height());
        // SAFETY: `as_hal` only asks that the texture outlives the borrow.
        let hal = unsafe { texture.as_hal::<wgpu::hal::api::Vulkan>() }
            .context("the interface texture is not Vulkan")?;
        // SAFETY: the texture is alive and the caller keeps it alive while the
        // view exists; the view is destroyed here or in `Drop`.
        let image = unsafe { hal.raw_handle() };
        self.ui_size = size;

        // SAFETY: the image belongs to this device; the view is destroyed here
        // or in `Drop`, and wgpu keeps the image alive for the caller.
        let view = unsafe {
            self.device.create_image_view(
                &vk::ImageViewCreateInfo::default()
                    .image(image)
                    .view_type(vk::ImageViewType::TYPE_2D)
                    .format(UI_FORMAT)
                    .subresource_range(color_range()),
                None,
            )
        }
        .context("interface image view")?;

        let layouts = [self.ui_layout];
        // SAFETY: the layout and pool are live and the set is freed below.
        let sets = unsafe {
            self.device.allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(self.pool)
                    .set_layouts(&layouts),
            )
        }
        .context("interface descriptor set")?;
        let ui_set = sets[0];

        let info = vk::DescriptorImageInfo::default()
            .sampler(self.sampler)
            .image_view(view)
            .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
        let write = vk::WriteDescriptorSet::default()
            .dst_set(ui_set)
            .dst_binding(0)
            .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
            .image_info(std::slice::from_ref(&info));
        // SAFETY: the set belongs to this pool and the write names a live view.
        unsafe { self.device.update_descriptor_sets(&[write], &[]) };

        // SAFETY: the old view and set were created here and are replaced now.
        unsafe {
            if !self.ui_set.is_null() {
                self.device
                    .free_descriptor_sets(self.pool, &[self.ui_set])
                    .ok();
            }
            if !self.ui_view.is_null() {
                self.device.destroy_image_view(self.ui_view, None);
            }
        }
        self.ui_view = view;
        self.ui_set = ui_set;
        Ok(())
    }

    /// Rebuilds the per-backbuffer views, framebuffers and scan sets.
    pub fn set_targets(
        &mut self,
        images: &[vk::Image],
        width: u32,
        height: u32,
    ) -> eros::Result<()> {
        self.clear_targets();
        for image in images {
            // SAFETY: the image belongs to this device and the format matches
            // the swapchain it came from.
            let view = unsafe {
                self.device.create_image_view(
                    &vk::ImageViewCreateInfo::default()
                        .image(*image)
                        .view_type(vk::ImageViewType::TYPE_2D)
                        .format(self.format)
                        .subresource_range(color_range()),
                    None,
                )
            }
            .context("backbuffer image view")?;
            // SAFETY: the render pass and view are live and agree on the extent.
            let framebuffer = unsafe {
                self.device.create_framebuffer(
                    &vk::FramebufferCreateInfo::default()
                        .render_pass(self.render)
                        .attachments(std::slice::from_ref(&view))
                        .width(width)
                        .height(height)
                        .layers(1),
                    None,
                )
            }
            .context("backbuffer framebuffer")?;

            let layouts = [self.scan_set_layout];
            // SAFETY: the layout and pool are live.
            let sets = unsafe {
                self.device.allocate_descriptor_sets(
                    &vk::DescriptorSetAllocateInfo::default()
                        .descriptor_pool(self.pool)
                        .set_layouts(&layouts),
                )
            }
            .context("scan descriptor set")?;
            let scan_set = sets[0];
            let image_info = vk::DescriptorImageInfo::default()
                .sampler(self.sampler)
                .image_view(view)
                .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL);
            let buffer_info = vk::DescriptorBufferInfo::default()
                .buffer(self.report_buffer)
                .offset(0)
                .range(vk::WHOLE_SIZE);
            let writes = [
                vk::WriteDescriptorSet::default()
                    .dst_set(scan_set)
                    .dst_binding(0)
                    .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                    .image_info(std::slice::from_ref(&image_info)),
                vk::WriteDescriptorSet::default()
                    .dst_set(scan_set)
                    .dst_binding(1)
                    .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                    .buffer_info(std::slice::from_ref(&buffer_info)),
            ];
            // SAFETY: the set is live, as are the view and buffer it names.
            unsafe { self.device.update_descriptor_sets(&writes, &[]) };

            self.targets.push(Target {
                image: *image,
                view,
                framebuffer,
                scan_set,
            });
        }
        Ok(())
    }

    /// Records this frame into `command`: the interface over the fill, then the
    /// scan that counts what the fill covered.
    pub fn record(
        &self,
        command: vk::CommandBuffer,
        index: usize,
        ui: vk::Image,
        extent: (u32, u32),
        hdr: bool,
        white: f32,
    ) {
        let Some(target) = self.targets.get(index) else {
            return;
        };
        let frame = self.frame.get() + 1;
        self.frame.set(frame);
        let (width, height) = extent;

        // The interface was last written as a render attachment by ZGUI; this
        // pass reads it. It is put back the way ZGUI left it afterwards.
        let to_read = vk::ImageMemoryBarrier::default()
            .old_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
            .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
            .src_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)
            .dst_access_mask(vk::AccessFlags::SHADER_READ)
            .image(ui)
            .subresource_range(color_range())
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED);
        let to_write = vk::ImageMemoryBarrier::default()
            .old_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
            .new_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
            .src_access_mask(vk::AccessFlags::SHADER_READ)
            .dst_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)
            .image(ui)
            .subresource_range(color_range())
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED);
        // SAFETY: `command` is recording and both images are live.
        unsafe {
            self.device.cmd_pipeline_barrier(
                command,
                vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                vk::PipelineStageFlags::FRAGMENT_SHADER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[to_read],
            );
        }

        let clears = [vk::ClearValue {
            color: vk::ClearColorValue { float32: FILL },
        }];
        let area = vk::Rect2D {
            offset: vk::Offset2D { x: 0, y: 0 },
            extent: vk::Extent2D { width, height },
        };
        let begin = vk::RenderPassBeginInfo::default()
            .render_pass(self.render)
            .framebuffer(target.framebuffer)
            .render_area(area)
            .clear_values(&clears);

        let mut push = [0u8; 24];
        push[0..4].copy_from_slice(&(self.ui_size.0 as f32).to_ne_bytes());
        push[4..8].copy_from_slice(&(self.ui_size.1 as f32).to_ne_bytes());
        push[8..12].copy_from_slice(&u32::from(hdr).to_ne_bytes());
        push[12..16].copy_from_slice(&white.to_ne_bytes());

        // SAFETY: the framebuffer, pipeline and set all belong to this device
        // and the viewport/scissor cover the attachment exactly.
        unsafe {
            self.device.cmd_begin_render_pass(
                command,
                &begin,
                vk::SubpassContents::INLINE,
            );
            self.device
                .cmd_bind_pipeline(command, vk::PipelineBindPoint::GRAPHICS, self.pipeline);
            self.device.cmd_bind_descriptor_sets(
                command,
                vk::PipelineBindPoint::GRAPHICS,
                self.pipeline_layout,
                0,
                &[self.ui_set],
                &[],
            );
            self.device.cmd_push_constants(
                command,
                self.pipeline_layout,
                vk::ShaderStageFlags::FRAGMENT,
                0,
                &push[..size_of::<UiParams>()],
            );
            self.device.cmd_set_viewport(
                command,
                0,
                &[vk::Viewport {
                    x: 0.0,
                    y: 0.0,
                    width: width as f32,
                    height: height as f32,
                    min_depth: 0.0,
                    max_depth: 1.0,
                }],
            );
            self.device.cmd_set_scissor(command, 0, &[area]);
            self.device.cmd_draw(command, 3, 1, 0, 0);
            self.device.cmd_end_render_pass(command);

            // ZGUI's texture goes back to the layout its queue believes it is in.
            self.device.cmd_pipeline_barrier(
                command,
                vk::PipelineStageFlags::FRAGMENT_SHADER,
                vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[to_write],
            );

            // The scan reads the finished frame.
            let to_scan = vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
                .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                .src_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)
                .dst_access_mask(vk::AccessFlags::SHADER_READ)
                .image(target.image)
                .subresource_range(color_range())
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED);
            self.device.cmd_pipeline_barrier(
                command,
                vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                vk::PipelineStageFlags::COMPUTE_SHADER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[to_scan],
            );

            // A fresh tally: no count, and rows that no invocation has claimed.
            self.device.cmd_fill_buffer(command, self.report_buffer, 4, 4, 0);
            self.device
                .cmd_fill_buffer(command, self.report_buffer, 8, 4, u32::MAX);
            self.device.cmd_fill_buffer(command, self.report_buffer, 12, 4, 0);

            self.device
                .cmd_bind_pipeline(command, vk::PipelineBindPoint::COMPUTE, self.scan_pipeline);
            self.device.cmd_bind_descriptor_sets(
                command,
                vk::PipelineBindPoint::COMPUTE,
                self.scan_layout,
                0,
                &[target.scan_set],
                &[],
            );
            let mut scan = [0u8; 12];
            scan[0..4].copy_from_slice(&u32::try_from(frame).unwrap_or(u32::MAX).to_ne_bytes());
            scan[4..8].copy_from_slice(&width.to_ne_bytes());
            scan[8..12].copy_from_slice(&height.to_ne_bytes());
            self.device.cmd_push_constants(
                command,
                self.scan_layout,
                vk::ShaderStageFlags::COMPUTE,
                0,
                &scan,
            );
            let groups = (width * height).div_ceil(64);
            self.device.cmd_dispatch(command, groups, 1, 1);
        }
    }

    /// The last band the scan reported, if a new frame has one.
    #[must_use]
    pub fn band(&self) -> Option<(u64, u32, u32, u32)> {
        // SAFETY: the buffer is mapped for the life of the pass. The CPU reads
        // it while the GPU may be writing; a torn read is harmless here and is
        // exactly the price of not synchronising.
        let (frame, count, first, last) = unsafe {
            (
                ptr::read_volatile(self.report),
                ptr::read_volatile(self.report.add(1)),
                ptr::read_volatile(self.report.add(2)),
                ptr::read_volatile(self.report.add(3)),
            )
        };
        let frame = u64::from(frame);
        if count == 0 || frame == self.reported.get() {
            return None;
        }
        self.reported.set(frame);
        Some((frame, count, first, last))
    }

    fn clear_targets(&mut self) {
        for target in self.targets.drain(..) {
            // SAFETY: every object was created here and is destroyed once.
            unsafe {
                self.device
                    .free_descriptor_sets(self.pool, &[target.scan_set])
                    .ok();
                self.device.destroy_framebuffer(target.framebuffer, None);
                self.device.destroy_image_view(target.view, None);
            }
        }
    }
}

impl Drop for Pass {
    fn drop(&mut self) {
        self.clear_targets();
        // SAFETY: every object below was created by this struct and the mapped
        // range is unmapped exactly once.
        unsafe {
            if !self.ui_set.is_null() {
                self.device
                    .free_descriptor_sets(self.pool, &[self.ui_set])
                    .ok();
            }
            if !self.ui_view.is_null() {
                self.device.destroy_image_view(self.ui_view, None);
            }
            self.device.unmap_memory(self.report_memory);
            self.device.destroy_buffer(self.report_buffer, None);
            self.device.free_memory(self.report_memory, None);
            self.device.destroy_sampler(self.sampler, None);
            self.device.destroy_pipeline(self.scan_pipeline, None);
            self.device.destroy_pipeline_layout(self.scan_layout, None);
            self.device.destroy_pipeline(self.pipeline, None);
            self.device.destroy_pipeline_layout(self.pipeline_layout, None);
            self.device.destroy_descriptor_pool(self.pool, None);
            self.device
                .destroy_descriptor_set_layout(self.scan_set_layout, None);
            self.device.destroy_descriptor_set_layout(self.ui_layout, None);
            self.device.destroy_render_pass(self.render, None);
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct UiParams {
    ui_size: [f32; 2],
    mode: u32,
    white: f32,
}

const fn color_range() -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange {
        aspect_mask: vk::ImageAspectFlags::COLOR,
        base_mip_level: 0,
        level_count: 1,
        base_array_layer: 0,
        layer_count: 1,
    }
}

fn render_pass(device: &ash::Device, format: vk::Format) -> eros::Result<vk::RenderPass> {
    let attachment = vk::AttachmentDescription::default()
        .format(format)
        .samples(vk::SampleCountFlags::TYPE_1)
        .load_op(vk::AttachmentLoadOp::CLEAR)
        .store_op(vk::AttachmentStoreOp::STORE)
        .stencil_load_op(vk::AttachmentLoadOp::DONT_CARE)
        .stencil_store_op(vk::AttachmentStoreOp::DONT_CARE)
        // Everything is cleared, so the previous contents are never read: the
        // first frame over a fresh image and the tenth cost the same.
        .initial_layout(vk::ImageLayout::UNDEFINED)
        .final_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL);
    let reference = [vk::AttachmentReference::default()
        .attachment(0)
        .layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)];
    let subpass = [vk::SubpassDescription::default()
        .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
        .color_attachments(&reference)];
    let dependency = [vk::SubpassDependency::default()
        .src_subpass(vk::SUBPASS_EXTERNAL)
        .dst_subpass(0)
        .src_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
        .dst_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
        .src_access_mask(vk::AccessFlags::empty())
        .dst_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)];
    let attachments = [attachment];
    // SAFETY: the descriptions are valid locals and name this device's format.
    let pass = unsafe {
        device.create_render_pass(
            &vk::RenderPassCreateInfo::default()
                .attachments(&attachments)
                .subpasses(&subpass)
                .dependencies(&dependency),
            None,
        )
    }
    .context("vkCreateRenderPass")?;
    Ok(pass)
}

fn descriptor_layout(
    device: &ash::Device,
    bindings: &[(u32, vk::DescriptorType, vk::ShaderStageFlags)],
) -> eros::Result<vk::DescriptorSetLayout> {
    let entries: Vec<vk::DescriptorSetLayoutBinding<'_>> = bindings
        .iter()
        .map(|(binding, ty, stage)| {
            vk::DescriptorSetLayoutBinding::default()
                .binding(*binding)
                .descriptor_type(*ty)
                .descriptor_count(1)
                .stage_flags(*stage)
        })
        .collect();
    // SAFETY: the bindings are valid locals.
    let layout = unsafe {
        device.create_descriptor_set_layout(
            &vk::DescriptorSetLayoutCreateInfo::default().bindings(&entries),
            None,
        )
    }
    .context("vkCreateDescriptorSetLayout")?;
    Ok(layout)
}

fn make_pipeline_layout(
    device: &ash::Device,
    set: vk::DescriptorSetLayout,
    stage: vk::ShaderStageFlags,
) -> eros::Result<vk::PipelineLayout> {
    let sets = [set];
    let ranges = [vk::PushConstantRange::default()
        .stage_flags(stage)
        .offset(0)
        .size(size_of::<UiParams>() as u32)];
    // SAFETY: the layout and range are valid locals.
    let layout = unsafe {
        device.create_pipeline_layout(
            &vk::PipelineLayoutCreateInfo::default()
                .set_layouts(&sets)
                .push_constant_ranges(&ranges),
            None,
        )
    }
    .context("vkCreatePipelineLayout")?;
    Ok(layout)
}

fn shader(device: &ash::Device, bytes: &[u8]) -> eros::Result<vk::ShaderModule> {
    eros::ensure!(bytes.len().is_multiple_of(4), "shader is not SPIR-V");
    // `include_bytes!` is a byte slice with byte alignment, and `VkShaderModule`
    // takes words: copy once, at startup, rather than reinterpret.
    let words: Vec<u32> = bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|word| u32::from_ne_bytes(*word))
        .collect();
    // SAFETY: the words are the shader's own bytes and outlive the call.
    let module = unsafe {
        device.create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&words), None)
    }
    .context("vkCreateShaderModule")?;
    Ok(module)
}

fn graphics_pipeline(
    device: &ash::Device,
    render: vk::RenderPass,
    layout: vk::PipelineLayout,
) -> eros::Result<vk::Pipeline> {
    let vertex = shader(device, include_bytes!(concat!(env!("OUT_DIR"), "/ui.vert.spv")))?;
    let fragment = shader(device, include_bytes!(concat!(env!("OUT_DIR"), "/ui.frag.spv")))?;
    let stages = [
        vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::VERTEX)
            .module(vertex)
            .name(c"main"),
        vk::PipelineShaderStageCreateInfo::default()
            .stage(vk::ShaderStageFlags::FRAGMENT)
            .module(fragment)
            .name(c"main"),
    ];
    let viewport = [vk::Viewport::default()];
    let scissor = [vk::Rect2D::default()];
    let viewports = vk::PipelineViewportStateCreateInfo::default()
        .viewports(&viewport)
        .scissors(&scissor);
    let vertex_input = vk::PipelineVertexInputStateCreateInfo::default();
    let assembly = vk::PipelineInputAssemblyStateCreateInfo::default()
        .topology(vk::PrimitiveTopology::TRIANGLE_LIST);
    let raster = vk::PipelineRasterizationStateCreateInfo::default()
        .polygon_mode(vk::PolygonMode::FILL)
        .cull_mode(vk::CullModeFlags::NONE)
        .line_width(1.0);
    let multisample = vk::PipelineMultisampleStateCreateInfo::default()
        .rasterization_samples(vk::SampleCountFlags::TYPE_1);
    // The shader does the compositing, so the blending is plain write.
    let blend = [vk::PipelineColorBlendAttachmentState::default()
        .color_write_mask(vk::ColorComponentFlags::RGBA)
        .blend_enable(false)];
    let blending = vk::PipelineColorBlendStateCreateInfo::default().attachments(&blend);
    let dynamic = [vk::DynamicState::VIEWPORT, vk::DynamicState::SCISSOR];
    let dynamic_state = vk::PipelineDynamicStateCreateInfo::default().dynamic_states(&dynamic);
    let info = vk::GraphicsPipelineCreateInfo::default()
        .stages(&stages)
        .vertex_input_state(&vertex_input)
        .input_assembly_state(&assembly)
        .viewport_state(&viewports)
        .rasterization_state(&raster)
        .multisample_state(&multisample)
        .color_blend_state(&blending)
        .dynamic_state(&dynamic_state)
        .layout(layout)
        .render_pass(render);
    // SAFETY: every structure above is a live local naming this device's objects.
    let pipelines = match unsafe {
        device.create_graphics_pipelines(vk::PipelineCache::null(), &[info], None)
    } {
        Ok(pipelines) => pipelines,
        Err((_, error)) => return Err(error).union().context("vkCreateGraphicsPipelines"),
    };
    // SAFETY: the modules were only needed to create the pipeline.
    unsafe {
        device.destroy_shader_module(vertex, None);
        device.destroy_shader_module(fragment, None);
    }
    Ok(pipelines[0])
}

fn compute_pipeline(device: &ash::Device, layout: vk::PipelineLayout) -> eros::Result<vk::Pipeline> {
    let module = shader(
        device,
        include_bytes!(concat!(env!("OUT_DIR"), "/scan.comp.spv")),
    )?;
    let stage = vk::PipelineShaderStageCreateInfo::default()
        .stage(vk::ShaderStageFlags::COMPUTE)
        .module(module)
        .name(c"main");
    let info = vk::ComputePipelineCreateInfo::default()
        .stage(stage)
        .layout(layout);
    // SAFETY: the stage and layout are live locals.
    let pipelines =
        match unsafe { device.create_compute_pipelines(vk::PipelineCache::null(), &[info], None) } {
            Ok(pipelines) => pipelines,
            Err((_, error)) => return Err(error).union().context("vkCreateComputePipelines"),
        };
    // SAFETY: the module was only needed to create the pipeline.
    unsafe { device.destroy_shader_module(module, None) };
    Ok(pipelines[0])
}

fn sampler(device: &ash::Device) -> eros::Result<vk::Sampler> {
    // SAFETY: the description is a valid local. Clamping is what edge-extends
    // the interface over the rest of a canvas larger than it.
    let handle = unsafe {
        device.create_sampler(
            &vk::SamplerCreateInfo::default()
                .mag_filter(vk::Filter::LINEAR)
                .min_filter(vk::Filter::LINEAR)
                .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE),
            None,
        )
    }
    .context("vkCreateSampler")?;
    Ok(handle)
}

fn report_buffer(
    device: &ash::Device,
    hal: &wgpu::hal::vulkan::Device,
) -> eros::Result<(vk::Buffer, vk::DeviceMemory, *mut u32)> {
    // SAFETY: the buffer is created on this device and destroyed by `Drop`.
    let buffer = unsafe {
        device.create_buffer(
            &vk::BufferCreateInfo::default()
                .size(16)
                .usage(vk::BufferUsageFlags::STORAGE_BUFFER | vk::BufferUsageFlags::TRANSFER_DST),
            None,
        )
    }
    .context("vkCreateBuffer(report)")?;
    // SAFETY: the buffer was just created.
    let requirements = unsafe { device.get_buffer_memory_requirements(buffer) };
    // SAFETY: the physical device belongs to this instance and is live.
    let properties = unsafe {
        hal.shared_instance()
            .raw_instance()
            .get_physical_device_memory_properties(hal.raw_physical_device())
    };
    let wanted = vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
    let memory_type = (0..properties.memory_type_count)
        .find(|index| {
            requirements.memory_type_bits & (1 << index) != 0
                && properties.memory_types[*index as usize]
                    .property_flags
                    .contains(wanted)
        })
        .context("no host-visible memory type for the report buffer")?;
    // SAFETY: the allocation names the buffer's own requirements.
    let memory = unsafe {
        device.allocate_memory(
            &vk::MemoryAllocateInfo::default()
                .allocation_size(requirements.size)
                .memory_type_index(memory_type),
            None,
        )
    }
    .context("vkAllocateMemory(report)")?;
    // SAFETY: the memory was just allocated for this buffer.
    if let Err(error) = unsafe { device.bind_buffer_memory(buffer, memory, 0) } {
        // SAFETY: both are ours and nothing else names them.
        unsafe {
            device.destroy_buffer(buffer, None);
            device.free_memory(memory, None);
        }
        return Err(error).union().context("vkBindBufferMemory(report)");
    }
    // SAFETY: the memory is host visible and coherent, and is unmapped once.
    let pointer = unsafe { device.map_memory(memory, 0, 16, vk::MemoryMapFlags::empty()) }
        .context("vkMapMemory(report)")?;
    Ok((buffer, memory, pointer.cast::<u32>()))
}
