//! `vk_dxgi`'s blit path: shared canvases, copied into the backbuffer by D3D12.
//!
//! The share path — Vulkan rendering straight into the imported backbuffer —
//! leaves DXGI with nothing to order the scanout against, because the app's
//! D3D12 queue never executes a command list. Vulkan renders into a canvas that
//! D3D12 owns instead, and every frame ends with a real command list: copy the
//! canvas into the current backbuffer, barriered to `PRESENT`. The composite is
//! then ordered the way a D3D12 application's would be, and the backbuffer is
//! only ever touched by D3D12.
//!
//! There is one canvas per backbuffer, so the canvas a frame renders into is the
//! one whose copy went out three frames ago. Waiting on a copy issued on the
//! previous frame means waiting on a signal the other API has not reached yet,
//! which costs about a tenth of a second on this driver; waiting on the copy
//! that used *this* canvas three frames ago costs nothing, because it landed
//! long before the render starts.

use ash::vk;
use eros::Context as _;
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::Foundation::GENERIC_ALL;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Graphics::Direct3D12::D3D12_HEAP_FLAG_SHARED;
use windows::Win32::Graphics::Direct3D12::D3D12_HEAP_PROPERTIES;
use windows::Win32::Graphics::Direct3D12::D3D12_HEAP_TYPE_DEFAULT;
use windows::Win32::Graphics::Direct3D12::D3D12_RESOURCE_BARRIER;
use windows::Win32::Graphics::Direct3D12::D3D12_RESOURCE_BARRIER_0;
use windows::Win32::Graphics::Direct3D12::D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES;
use windows::Win32::Graphics::Direct3D12::D3D12_RESOURCE_BARRIER_FLAG_NONE;
use windows::Win32::Graphics::Direct3D12::D3D12_RESOURCE_BARRIER_TYPE_TRANSITION;
use windows::Win32::Graphics::Direct3D12::D3D12_RESOURCE_DESC;
use windows::Win32::Graphics::Direct3D12::D3D12_RESOURCE_DIMENSION_TEXTURE2D;
use windows::Win32::Graphics::Direct3D12::D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET;
use windows::Win32::Graphics::Direct3D12::D3D12_RESOURCE_STATE_COMMON;
use windows::Win32::Graphics::Direct3D12::D3D12_RESOURCE_STATE_COPY_DEST;
use windows::Win32::Graphics::Direct3D12::D3D12_RESOURCE_STATE_PRESENT;
use windows::Win32::Graphics::Direct3D12::D3D12_RESOURCE_STATES;
use windows::Win32::Graphics::Direct3D12::D3D12_RESOURCE_TRANSITION_BARRIER;
use windows::Win32::Graphics::Direct3D12::D3D12_TEXTURE_COPY_LOCATION;
use windows::Win32::Graphics::Direct3D12::D3D12_TEXTURE_COPY_LOCATION_0;
use windows::Win32::Graphics::Direct3D12::D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX;
use windows::Win32::Graphics::Direct3D12::D3D12_TEXTURE_LAYOUT_UNKNOWN;
use windows::Win32::Graphics::Direct3D12::D3D12_COMMAND_LIST_TYPE_DIRECT;
use windows::Win32::Graphics::Direct3D12::ID3D12CommandAllocator;
use windows::Win32::Graphics::Direct3D12::ID3D12CommandList;
use windows::Win32::Graphics::Direct3D12::ID3D12DeviceChild;
use windows::Win32::Graphics::Direct3D12::ID3D12GraphicsCommandList;
use windows::Win32::Graphics::Direct3D12::ID3D12Resource;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R10G10B10A2_UNORM;
use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;
use windows::core::Interface as _;
use windows::core::PCWSTR;
use zgui_render_wgpu::Gpu;

use crate::dxgi::BUFFERS;
use crate::dxgi::D3d12;
use crate::dxgi::VK_FORMAT;

/// One canvas per backbuffer.
///
/// Canvas `i` is only ever used by a frame presenting into backbuffer `i`, so
/// its previous copy is three frames old by the time it is rendered into again,
/// and the copy source and destination always pair up.
pub const CANVASES: usize = BUFFERS as usize;

/// One canvas: a D3D12 resource, its shared handle, and the Vulkan image over
/// it.
struct Canvas {
    resource: ID3D12Resource,
    handle: HANDLE,
    image: vk::Image,
    memory: vk::DeviceMemory,
}

/// The canvases D3D12 owns and Vulkan renders into, plus the lists that copy
/// them out.
pub struct Blit {
    canvases: Vec<Canvas>,
    device: ash::Device,
    /// One allocator and list per backbuffer: a buffer is only handed out again
    /// once its frame retired, which is also when its list is done.
    lists: Vec<(ID3D12CommandAllocator, ID3D12GraphicsCommandList)>,
}

impl Blit {
    pub fn new(d3d12: &D3d12, gpu: &Gpu, width: u32, height: u32) -> eros::Result<Self> {
        eros::ensure!(width > 0 && height > 0, "zero-sized canvas");

        // SAFETY: `as_hal` only asks that the resource behind the guard outlives
        // the borrow. Both wgpu handles are alive for the whole call.
        let hal_adapter =
            unsafe { gpu.adapter().as_hal::<zgui_render_wgpu::wgpu::hal::api::Vulkan>() }
                .context("the video device is not Vulkan")?;
        // SAFETY: as above.
        let hal_device =
            unsafe { gpu.device().as_hal::<zgui_render_wgpu::wgpu::hal::api::Vulkan>() }
                .context("the video device is not Vulkan")?;
        let instance = hal_adapter.shared_instance().raw_instance();

        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Alignment: 0,
            Width: u64::from(width),
            Height: height,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: DXGI_FORMAT_R10G10B10A2_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_UNKNOWN,
            Flags: D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
        };
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_DEFAULT,
            ..Default::default()
        };

        let mut canvases = Vec::with_capacity(CANVASES);
        for _ in 0..CANVASES {
            let mut created: Option<ID3D12Resource> = None;
            // SAFETY: the device is live and both descriptors are valid locals;
            // the out-parameter names a live local.
            unsafe {
                d3d12.device.CreateCommittedResource(
                    &heap,
                    D3D12_HEAP_FLAG_SHARED,
                    &desc,
                    D3D12_RESOURCE_STATE_COMMON,
                    None,
                    &mut created,
                )
            }
            .context("CreateCommittedResource(canvas)")?;
            let resource = created.context("CreateCommittedResource returned no resource")?;

            let child: ID3D12DeviceChild = resource.cast().context("canvas device child")?;
            // SAFETY: the resource is a D3D12 texture and `GENERIC_ALL` is what
            // the reference interop path asks for.
            let handle = unsafe {
                d3d12
                    .device
                    .CreateSharedHandle(&child, None, GENERIC_ALL.0, PCWSTR::null())
            }
            .context("CreateSharedHandle(canvas)")?;

            // SAFETY: the handle names a texture of this shape on this device.
            let (image, memory) = match unsafe {
                crate::dxgi::import_shared(
                    &hal_device,
                    instance,
                    handle,
                    width,
                    height,
                    VK_FORMAT,
                )
            } {
                Ok(imported) => imported,
                Err(error) => {
                    // SAFETY: the handle came from `CreateSharedHandle` above.
                    unsafe {
                        let _ = CloseHandle(handle);
                    }
                    return Err(error);
                }
            };

            canvases.push(Canvas {
                resource,
                handle,
                image,
                memory,
            });
        }

        let mut lists = Vec::with_capacity(BUFFERS as usize);
        for _ in 0..BUFFERS {
            // SAFETY: the device is live; both objects are released in `Drop`.
            unsafe {
                let allocator: ID3D12CommandAllocator = d3d12
                    .device
                    .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
                    .context("CreateCommandAllocator(copy)")?;
                let list: ID3D12GraphicsCommandList = d3d12
                    .device
                    .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)
                    .context("CreateCommandList(copy)")?;
                // Recorded from scratch every frame.
                list.Close().context("command list close")?;
                lists.push((allocator, list));
            }
        }

        Ok(Self {
            canvases,
            device: hal_device.raw_device().clone(),
            lists,
        })
    }

    /// The Vulkan image of canvas `slot`.
    #[must_use]
    pub fn image(&self, slot: usize) -> Option<vk::Image> {
        self.canvases.get(slot).map(|canvas| canvas.image)
    }

    /// Records and submits the copy from canvas `slot` into `backbuffer`.
    ///
    /// The backbuffer is transitioned to `COPY_DEST` and back to `PRESENT` in
    /// the same list, which is the state DXGI presents from. Nothing here waits
    /// on the CPU: the caller has already queued the wait for the render.
    pub fn copy(
        &self,
        d3d12: &D3d12,
        backbuffer: &ID3D12Resource,
        slot: usize,
        index: u32,
    ) -> eros::Result<()> {
        let Some(canvas) = self.canvases.get(slot) else {
            return Ok(());
        };
        let Some((allocator, list)) = self.lists.get(usize::try_from(index).unwrap_or(0)) else {
            return Ok(());
        };
        // SAFETY: the allocator, list and resources are live, and this buffer's
        // frame has retired so its list is idle; the queue is the one the
        // swapchain was created with, so the present is ordered after this list.
        unsafe {
            allocator.Reset().context("command allocator reset")?;
            list.Reset(allocator, None).context("command list reset")?;

            // Every one of these structures holds its resource in a
            // `ManuallyDrop`, so the clone it needs is never released by
            // dropping the structure: it is released explicitly below. Leaking
            // it leaves DXGI with outstanding references and makes every later
            // `ResizeBuffers` fail.
            let mut to_copy = transition(
                backbuffer,
                D3D12_RESOURCE_STATE_PRESENT,
                D3D12_RESOURCE_STATE_COPY_DEST,
            );
            list.ResourceBarrier(std::slice::from_ref(&to_copy));
            release_barrier(&mut to_copy);

            let mut dst = D3D12_TEXTURE_COPY_LOCATION {
                pResource: std::mem::ManuallyDrop::new(Some(backbuffer.clone())),
                Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 0 },
            };
            let mut src = D3D12_TEXTURE_COPY_LOCATION {
                pResource: std::mem::ManuallyDrop::new(Some(canvas.resource.clone())),
                Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 0 },
            };
            list.CopyTextureRegion(&dst, 0, 0, 0, &src, None);
            // The references the copy needed go now, not at the next resize.
            std::mem::ManuallyDrop::drop(&mut dst.pResource);
            std::mem::ManuallyDrop::drop(&mut src.pResource);

            let mut to_present = transition(
                backbuffer,
                D3D12_RESOURCE_STATE_COPY_DEST,
                D3D12_RESOURCE_STATE_PRESENT,
            );
            list.ResourceBarrier(std::slice::from_ref(&to_present));
            release_barrier(&mut to_present);

            list.Close().context("command list close")?;
            let commands: [Option<ID3D12CommandList>; 1] =
                [Some(list.cast().context("copy list cast")?)];
            d3d12.queue.ExecuteCommandLists(&commands);
        }
        Ok(())
    }
}

impl Drop for Blit {
    fn drop(&mut self) {
        for canvas in &self.canvases {
            // SAFETY: every object was created on this device and is released
            // once.
            unsafe {
                self.device.destroy_image(canvas.image, None);
                self.device.free_memory(canvas.memory, None);
                let _ = CloseHandle(canvas.handle);
            }
        }
    }
}

/// Releases the resource reference a transition barrier was given.
///
/// `pResource` lives in a `ManuallyDrop` because it shares its storage with the
/// union's other variants, so nothing releases it on its own.
fn release_barrier(barrier: &mut D3D12_RESOURCE_BARRIER) {
    // SAFETY: the barrier is in its transition variant — `transition` built it —
    // and the reference was cloned for the call that has already happened.
    unsafe {
        std::mem::ManuallyDrop::drop(&mut (*barrier.Anonymous.Transition).pResource);
    }
}

/// One whole-resource transition barrier.
fn transition(
    resource: &ID3D12Resource,
    before: D3D12_RESOURCE_STATES,
    after: D3D12_RESOURCE_STATES,
) -> D3D12_RESOURCE_BARRIER {
    D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            Transition: std::mem::ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                pResource: std::mem::ManuallyDrop::new(Some(resource.clone())),
                Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                StateBefore: before,
                StateAfter: after,
            }),
        },
    }
}
