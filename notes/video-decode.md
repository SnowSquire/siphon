# Video decode

Where decoded frames come from, per OS, and how they reach libplacebo without a
copy. The renderer does not change: `pl_render_image` still draws one `pl_frame`
per video frame into the swapchain (`notes/video-player.md` for the Windows
prototype, `notes/linux-port.md` for the Wayland chain).

**Decision: VA-API on Linux, D3D12 Video on Windows (D3D11VA only as a
probe-selected fallback). Vulkan Video is not used.**

## Vulkan Video is out

Measured on `shivering` (Intel N97, ADL-N `8086:46d1`, i915, Mesa 26.2.6,
Vulkan 1.4.354):

- ANV advertises a decode-capable queue family (`QUEUE_VIDEO_DECODE_BIT_KHR` is
  printed for it) and the `IMAGE_LAYOUT_VIDEO_DECODE_*` layouts, but **no**
  `VK_KHR_video_queue` / `VK_KHR_video_decode_h264|h265|av1` extension. The queue
  bit is core Vulkan 1.3; the codec extensions are what a decoder actually needs,
  and Mesa does not expose them for this GPU.
- `MESA_VK_ENABLE_VIDEO_DECODE=1` changes nothing (`notes/linux-port.md`
  recorded the same on 26.2.5).

That alone rules it out here. The wider argument holds too: Vulkan Video decode
would be a third backend to keep working per driver and per codec, on top of the
two the OS vendors already ship and test. Twitch is H.264, which both VA-API and
D3D11VA have covered since forever. The Vulkan decode path in the spike
(`crates/video/src/gpu.rs`, `hw.rs`, `AV_PIX_FMT_VULKAN`) goes away with the
spike.

## The box decodes; Fedora's driver build does not

The earlier "this box cannot decode" was a packaging artefact, not hardware:

- Fedora's `libva-intel-media-driver` is the **free** build. `vainfo` lists only
  MPEG2, VP8, VP9 and JPEG, and FFmpeg's VA-API path dies with
  `No support for codec h264 profile 100`.
- RPM Fusion's `intel-media-driver` (nonfree) installs
  `/usr/lib64/dri-nonfree/iHD_drv_video.so`, which libva prefers. With it,
  `vainfo` reports H.264 Main/High/ConstrainedBaseline, HEVC Main/Main10/Main12
  and AV1 decode, and a test H.264 MPEG-TS decodes cleanly.
- Media engine state is healthy: GuC 70.49.4 and HuC 7.9.3 loaded and
  authenticated, VP9 hardware decode worked even under the free driver.

So the capability probe at startup must name the package, not just fail:
"install intel-media-driver from RPM Fusion" is the whole fix for a Fedora box.

## Linux: VA-API, imported by libplacebo

FFmpeg decodes into `AV_PIX_FMT_VAAPI` surfaces on `/dev/dri/renderD128`, and
libplacebo maps them itself:

- `pl_test_pixfmt_caps` accepts `AV_PIX_FMT_VAAPI`/`AV_PIX_FMT_DRM_PRIME` when the
  GPU's `import_caps.tex` has `PL_HANDLE_DMA_BUF`
  (`src/include/libplacebo/utils/libav.h`, `libav_internal.h:647`).
- `pl_map_avframe_derived` runs `av_hwframe_map(…, AV_PIX_FMT_DRM_PRIME,
  AV_HWFRAME_MAP_DIRECT)` on any other hwaccel format, then
  `pl_map_avframe_drm` creates one `pl_tex` per plane with
  `.import_handle = PL_HANDLE_DMA_BUF`, the layer's fourcc and
  `object->format_modifier` (`libav_internal.h:1165-1205`).
- `pl_avframe_params.tex` is only required for non-hwdec formats, so the VA-API
  path passes the frame alone; `pl_unmap_avframe` after the render returns it.

**Device prerequisites**, both already present on ANV here:
`VK_EXT_external_memory_dma_buf` and `VK_EXT_image_drm_format_modifier`, plus the
`drmFormatModifier` feature. libplacebo only advertises DMA_BUF import when
`vk->GetImageDrmFormatModifierPropertiesEXT` exists (`src/vulkan/gpu.c:315`), and
it can only be enabled at device creation — which the UI already controls through
the wgpu-hal create callback used to add the FFmpeg extensions.

Risks to check with real frames: iHD exports surfaces with vendor tiling
modifiers, and `pl_fmt_has_modifier` must accept them for NV12; a rejected
modifier fails the map, which the decoder must treat as "this backend cannot feed
the renderer", not as a broken frame.

## Windows: D3D12 Video, D3D11VA as the fallback

D3D11VA was the first draft of this section and it was the wrong instinct: it
would make the Windows path *four* APIs (D3D11 decode, D3D12 swapchain, Vulkan
render, DXGI present) with two different interop flavours.

D3D12 Video is the coherent one, because `dxgi.rs` already owns everything it
needs:

- The D3D12 device is already created on the LUID-matched adapter
  (`D3D12CreateDevice` after `vulkan_luid`), and D3D12 resources are already
  imported into Vulkan by NT handle for the backbuffers
  (`ID3D12Resource::CreateSharedHandle` → `VK_EXTERNAL_MEMORY_HANDLE_TYPE_D3D12_RESOURCE_BIT`).
  Decoding into D3D12 means decode, present and import are one API above Vulkan.
- FFmpeg is built for exactly this: `AVD3D12VADeviceContext.device` accepts *our*
  `ID3D12Device` (no second device), and `heap_flags`/`resource_flags` on the
  device and frames context let the pool allocate with `D3D12_HEAP_FLAG_SHARED`,
  which is the one thing an exported texture must have.
- Frames arrive as `AVD3D12VAFrame { texture, subresource_index, sync_ctx }`,
  and `AVD3D12VASyncContext` is an `ID3D12Fence` + event + value — a real
  synchronisation primitive, importable into Vulkan as a semaphore
  (`VK_KHR_external_semaphore_win32`, `D3D12_FENCE`), rather than the keyed mutex
  a shared D3D11 texture forces.
- Import per plane is the same call as the dmabuf path: NV12/P010 is two planes,
  two NT handles, `pl_tex_create(.import_handle = PL_HANDLE_WIN32, .shared_mem = {
  .handle, .stride_w, .offset })`.

**Why a fallback still exists.** D3D12 Video decode has the narrowest driver
coverage of the three APIs — older GPUs and some IGP drivers expose VA/D3D11
decode and no `ID3D12VideoDevice`. So the probe asks the D3D12 video device for
H.264 8-bit decoder support first, and only falls back to `d3d11va` (same adapter,
matched by LUID, keyed mutex around the shared texture) when that fails. Both sit
behind the same seam, so the fallback is one more file, not another architecture.

Binding caveat: check whether `ffmpeg-the-third` exposes
`AV_HWDEVICE_TYPE_D3D12VA` / `AVD3D12VADeviceContext`. If it does not, hand-roll
the structs the way `hw.rs` already does for `AVVulkanDeviceContext` — the fields
above are the whole contract, and `heap_flags` in particular is newer than some
bindings.

## Software decode stays refused

Unchanged: no libavcodec H.264/HEVC decode on licensing grounds. A codec the GPU
cannot decode in hardware means no video, reported at open time — never a silent
software fallback.

## Shape in the code

Not in `crates/video`: that crate is a spike and is deleted once the UI plays
video (`notes/linux-port.md`). It lands in `crates/ui` as `decode/`, picked by
`#[cfg]` like `Chain`, not a trait object (only one backend is ever linked):

| item | linux | windows |
|---|---|---|
| `HwDevice::open()` | VA-API on the render node | D3D12 Video on the swapchain's device; D3D11VA only if unsupported |
| `frame → pl_frame` | `pl_map_avframe_ex` (+ libplacebo's DRM derivation) | NT handle per plane → `pl_tex_create` |
| `release()` | `pl_unmap_avframe` | signal/wait the D3D12 fence, unmap AVFrame |

Above the seam: demuxer, decoder, the stream clock from `player.rs`, and the
container-colour fix-up (`player.rs` — HEVC overwrites `avctx->color_*` from a VUI
that says nothing while the container says BT.2020/PQ). Audio stays out of scope.

## Milestones

1. `libplacebo-sys` Linux branch and the version shim, then a local libplacebo
   prefix (system 7.360.1 is below the hardcoded symbol; the shim removes the
   floor). libav support in libplacebo is unconditional when the headers are
   present, so nothing extra to enable.
2. Startup capability probe: VA-API profiles from FFmpeg plus libplacebo's
   `PL_HANDLE_DMA_BUF` import cap, with the RPM Fusion hint on failure.
3. Linux zero-copy path end to end: a captured TS file first, then a live channel
   once the transport question (`notes/architecture.md`) is settled.
4. Windows D3D12 Video on the existing device and import path, `d3d11va` as the
   probe-selected fallback, verified on the desktop against the Vulkan Video path
   it replaces.
5. Delete `crates/video`.

## Open

- Whether ANV accepts iHD's NV12 modifier on ADL-N. One frame answers it; if not,
  the fallback is a VA-API surface copy into a linear export, which is still one
  copy fewer than a host round trip.
- 10-bit: HEVC Main10/P010 for HDR, and whether the container metadata path
  carries over unchanged.
- Whether the player wants `AV_HWFRAME_MAP_DIRECT` only, or also a read mapping
  for the CPU-side colour probing that `player.rs` does today.
