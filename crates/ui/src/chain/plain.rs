//! Presenting ZGUI's texture without libplacebo.
//!
//! ZGUI composes into a wgpu texture, and the DXGI backbuffer is a D3D12
//! resource shared into the same Vulkan device. Both sides are Vulkan here, so
//! the interface is blitted from one VkImage to the other on wgpu's own queue
//! and presented: no overlay pass, no `pl_tex` handover, no colour management.
//!
//! This exists to give the present path a surface with nothing else in it. The
//! colour is a plain format conversion, so PQ/HDR grading is not part of it.

use ash::vk;
use eros::Context as _;
use zgui_render_wgpu::Gpu;
use zgui_render_wgpu::wgpu;

/// Blits `source` (the interface texture) into `target` (the backbuffer image),
/// leaving both in the layouts their owners expect.
pub fn blit(
    gpu: &Gpu,
    source: &wgpu::Texture,
    target: vk::Image,
    target_format: vk::Format,
) -> eros::Result<()> {
    // SAFETY: `as_hal` only asks that the resource outlives the borrow.
    let hal_device = unsafe { gpu.device().as_hal::<wgpu::hal::api::Vulkan>() }
        .context("the interface texture is not Vulkan")?;
    // SAFETY: as above.
    let hal_texture = unsafe { source.as_hal::<wgpu::hal::api::Vulkan>() }
        .context("the interface texture is not Vulkan")?;

    let device: &ash::Device = hal_device.raw_device();
    let family = hal_device.queue_family_index();
    let (width, height) = (source.width(), source.height());

    // SAFETY: the queue belongs to this device and family.
    let queue = unsafe { device.get_device_queue(family, 0) };
    // SAFETY: the texture is alive and owned by the caller for the call.
    let source_image = unsafe { hal_texture.raw_handle() };

    // SAFETY: every object below is created from live handles and destroyed
    // before this function returns.
    unsafe {
        let views = (
            device.create_image_view(
                &vk::ImageViewCreateInfo::default()
                    .image(source_image)
                    .view_type(vk::ImageViewType::TYPE_2D)
                    .format(vk::Format::B8G8R8A8_UNORM)
                    .subresource_range(color_range()),
                None,
            )?,
            device.create_image_view(
                &vk::ImageViewCreateInfo::default()
                    .image(target)
                    .view_type(vk::ImageViewType::TYPE_2D)
                    .format(target_format)
                    .subresource_range(color_range()),
                None,
            )?,
        );

        let pool = device.create_command_pool(
            &vk::CommandPoolCreateInfo::default()
                .queue_family_index(family)
                .flags(vk::CommandPoolCreateFlags::TRANSIENT),
            None,
        )?;
        let command = device.allocate_command_buffers(
            &vk::CommandBufferAllocateInfo::default()
                .command_pool(pool)
                .level(vk::CommandBufferLevel::PRIMARY)
                .command_buffer_count(1),
        )?[0];

        device.begin_command_buffer(
            command,
            &vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
        )?;

        // wgpu left the interface in the layout its render pass ended in.
        barrier(
            device,
            command,
            source_image,
            vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
            vk::AccessFlags::TRANSFER_READ,
        );
        // The target is new every resize and nothing has read it: discard.
        barrier(
            device,
            command,
            target,
            vk::ImageLayout::UNDEFINED,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            vk::AccessFlags::empty(),
            vk::AccessFlags::TRANSFER_WRITE,
        );

        device.cmd_blit_image(
            command,
            source_image,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            target,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            &[vk::ImageBlit {
                src_subresource: vk::ImageSubresourceLayers::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .layer_count(1),
                src_offsets: [
                    vk::Offset3D::default(),
                    vk::Offset3D {
                        x: i32::try_from(width).unwrap_or(0),
                        y: i32::try_from(height).unwrap_or(0),
                        z: 1,
                    },
                ],
                dst_subresource: vk::ImageSubresourceLayers::default()
                    .aspect_mask(vk::ImageAspectFlags::COLOR)
                    .layer_count(1),
                dst_offsets: [
                    vk::Offset3D::default(),
                    vk::Offset3D {
                        x: i32::try_from(width).unwrap_or(0),
                        y: i32::try_from(height).unwrap_or(0),
                        z: 1,
                    },
                ],
            }],
            vk::Filter::NEAREST,
        );

        // Back to the layout D3D12 presents from, and back to the one wgpu
        // expects to find its own texture in.
        barrier(
            device,
            command,
            target,
            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
            vk::ImageLayout::GENERAL,
            vk::AccessFlags::TRANSFER_WRITE,
            vk::AccessFlags::empty(),
        );
        barrier(
            device,
            command,
            source_image,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL,
            vk::AccessFlags::TRANSFER_READ,
            vk::AccessFlags::COLOR_ATTACHMENT_WRITE,
        );

        device.end_command_buffer(command)?;

        let fence = device.create_fence(&vk::FenceCreateInfo::default(), None)?;
        let commands = [command];
        let submit = vk::SubmitInfo::default().command_buffers(&commands);
        device.queue_submit(queue, &[submit], fence)?;
        device.wait_for_fences(&[fence], true, u64::MAX)?;
        device.destroy_fence(fence, None);
        device.destroy_command_pool(pool, None);
        device.destroy_image_view(views.0, None);
        device.destroy_image_view(views.1, None);
    }
    Ok(())
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

/// One image memory barrier covering the whole image.
unsafe fn barrier(
    device: &ash::Device,
    command: vk::CommandBuffer,
    image: vk::Image,
    from: vk::ImageLayout,
    to: vk::ImageLayout,
    src_access: vk::AccessFlags,
    dst_access: vk::AccessFlags,
) {
    let barrier = vk::ImageMemoryBarrier::default()
        .old_layout(from)
        .new_layout(to)
        .src_access_mask(src_access)
        .dst_access_mask(dst_access)
        .image(image)
        .subresource_range(color_range());
    // SAFETY: the caller owns the command buffer and the image lives past it.
    unsafe {
        device.cmd_pipeline_barrier(
            command,
            vk::PipelineStageFlags::ALL_COMMANDS,
            vk::PipelineStageFlags::ALL_COMMANDS,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[barrier],
        );
    }
}
