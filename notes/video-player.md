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

`cargo run --bin video -- --file <path>` needs nothing exported: the native side
is a libplacebo shared build in `C:/Users/alex/.local/libplacebo-shared`, FFmpeg
from scoop, the Vulkan SDK, and the xwin splat for the CRT, and
`crates/libplacebo-sys/build.rs` finds all four.

That crate is the whole native story:

* bindgen over `wrapper.h`, with the include paths handed to clang directly
  rather than through `INCLUDE`;
* `cc` with the compiler named (`clang-cl`, since this machine has no `cl.exe`)
  to compile `src/libav.c`, the one non-`static inline` part of libplacebo's
  libav support — that is what `pl_map_avframe_ex` needs;
* `-L`/`-l libplacebo` from the prefix, no pkg-config and no import-library
  rename;
* the DLL copied into `target/<profile>` after the build, because Windows
  resolves it before `main` and `cargo run` does not apply Cargo's `[env]` to
  the program it starts.

Every path is a knob with an environment override (`LIBPLACEBO_PREFIX`,
`FFMPEG_DIR`, `VULKAN_SDK`, `LIBPLACEBO_SPLAT`), so another layout only needs
those set. The only variable Cargo itself must supply is `FFMPEG_DIR`, for
`ffmpeg-sys-the-third`; it lives in `.cargo/config.toml`, because a dependency's
build script cannot see anything a build script here sets, and nothing else in
the workspace reads it.

`ffcine-libplacebo-sys` would have done this through pkg-config, which needs
`PKG_CONFIG_PATH`, `CC` and `INCLUDE` in the environment. Cargo's `[env]` would
then apply them to the whole workspace, where they would break the Twitch
crate's Linux builds — hence the local crate.

### libplacebo itself

The prefix is built once, by `scripts/build-libplacebo.sh`. The script exists
because the invocation is a pile of machine paths and two traps, not because the
build is interesting: it writes the `shaderc.pc` and the meson native file it
needs, then runs meson, ninja and `meson install`.

What is on, and why:

* **shaderc, not glslang.** libplacebo generates its shaders as GLSL text per
  pipeline and compiles them at runtime — `pl_pass_params.glsl_shader` is the
  only shader input, there is no SPIR-V entry point — so a GLSL→SPIR-V compiler
  is part of the runtime. shaderc is upstream's preferred one and the Vulkan SDK
  ships `shaderc_combined.lib`. One translation per unique pipeline, cached
  after: ~70 ms on the first frame.
* **`opengl=disabled`, `d3d11=disabled`.** Only the Vulkan backend is used.

What is off, and why:

* **`dovi`, `libdovi`.** Dolby Vision reshaping and its external RPU parser.
  The sources are HDR10; nothing here produces DV metadata, so the code is dead
  weight rather than a win.
* **`lcms`.** ICC profiles need lcms2. The display's colour comes from DXGI,
  not from a profile.
* **`xxhash`.** Replaces libplacebo's internal siphash for cache keys: hashing
  speed only, and libxxhash is not installed.

Two traps the script works around. Windows pkgconf rewrites a `.pc`'s `prefix`
from the file's own location, so `PKG_CONFIG_DONT_DEFINE_PREFIX=1` is required
for the generated `shaderc.pc`. And meson drops a pkg-config include directory
that is already in `INCLUDE`, which silently loses `shaderc/shaderc.h`, so
`INCLUDE` for this build is the CRT and Windows SDK only.
