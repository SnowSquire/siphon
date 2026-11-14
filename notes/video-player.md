# video — the Siphon player prototype

`crates/video` plays one local file in a ZGUI window with real HDR output. It is
the sketch of the eventual desktop app, not part of the Twitch client: Windows
only, no audio, one file, no seeking.

## One device, three users

`gpu.rs` creates the only Vulkan device, through wgpu, because wgpu cannot adopt
a device it did not create. The hal creation callback adds what wgpu never asks
for:

* `hostQueryReset` and `timelineSemaphore`, which `pl_vulkan_import` requires;
* the `VK_KHR_video_*` extensions and a queue in the driver's video-decode
  family. NVIDIA keeps that family away from graphics (family 3 on this box),
  so wgpu would never create a queue there, and FFmpeg could not decode.

FFmpeg adopts the same `VkDevice` through `AVVulkanDeviceContext` (`hw.rs`), so
decoded frames are `VkImage`s libplacebo samples directly. `pl_map_avframe_ex`
turns one into a `pl_frame`, semaphores included: nothing is copied to the host.

## Decode

`player.rs` owns the demuxer, the decoder and the container's colour
description. FFmpeg's HEVC decoder overwrites `avctx->color_*` from the VUI, and
this file's VUI says nothing while the Matroska tags say BT.2020/PQ, so each
frame gets the container's values where the decoder left them unspecified.
Without that, libplacebo treats 10-bit HDR as SDR BT.709 and the picture is
tone mapped from the wrong place.

Frames are decoded ahead on the stream clock: the runtime draws more often than
24 times a second, so one frame per draw would play several times too fast. One
frame is shown from its timestamp until the next one's.

## Render

`renderer.rs` is one libplacebo pass per frame:

* the decoded frame goes into the current DXGI backbuffer, in the display's own
  space — PQ/Rec.2020 at the panel's reported peak when HDR is on, BT.1886/709
  otherwise;
* ZGUI's interface texture is blended over it as a `pl_overlay`, so the SDR
  interface is tone mapped by the same code as the video, against the SDR white
  point Windows reports for that output;
* DXGI presents. There is no `VkSurfaceKHR` and no `VkSwapchainKHR`; the
  backbuffers are D3D12 resources imported into Vulkan (`dxgi.rs`).

`pl_gpu_finish` before every present is the coarse part: a fence or a DXGI
frame-latency waitable should replace it.

## Interface

`app.rs` builds the ZGUI view. Its texture is `Bgra8Unorm` and must be
`RENDER_ATTACHMENT | TEXTURE_BINDING`; `placebo.rs` wraps the same `VkImage` for
the overlay pass. wgpu renders it, libplacebo samples it, and the two keep
layout state separately — so every frame releases the texture to libplacebo
with the layout wgpu left it in, and takes it back with a transition to
`COLOR_ATTACHMENT_OPTIMAL` before wgpu draws the next frame.

The `surface` element is only a frame pump now: it presents a 1×1 transparent
texture once per frame interval, which is what asks the runtime for a frame.
The picture is underneath it.

## SDR white

`DISPLAYCONFIG_SDR_WHITE_LEVEL` from the DisplayConfig API, matched to the DXGI
output by GDI device name (`sdr.rs`). The value is Windows' multiplier on the
80-nit SDR reference in thousandths: 6000 is 480 nits. It sets the interface's
white point and any SDR video's, so an HDR window matches what SDR content looks
like on that display.

## Building

The native side is a libplacebo shared build plus FFmpeg, both found through
pkg-config:

```
export FFMPEG_DIR=C:/Users/alex/scoop/apps/ffmpeg-shared/current
export PKG_CONFIG_PATH=/c/Users/alex/AppData/Local/Temp/video-pkgconfig
export PKG_CONFIG=C:/Users/alex/scoop/shims/pkg-config.exe
export LIBCLANG_PATH=C:/Users/alex/scoop/apps/llvm/current/bin
export CC=clang-cl CXX=clang-cl AR=llvm-lib
```

`video-pkgconfig` holds hand-written `.pc` files for FFmpeg, Vulkan and the
libplacebo build in `C:/Users/alex/.local/libplacebo-shared` (whose import
library needs a `placebo.lib` copy for `-lplacebo`). The `INCLUDE`/`LIB`
environment must point at the xwin splat, glslang and the Vulkan SDK.
