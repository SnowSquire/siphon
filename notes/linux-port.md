# Linux port

Linux is where the UI gets iterated on away from the Windows desktop: same ZGUI
view, same daemon, same RPC. Nothing here changes what Windows does — the DXGI
path stays the reference implementation and every commit must leave it running
identically.

The port is in progress. What is in the tree today is step 1 (below): the crate
builds and runs on Linux, but presents through ZGUI's own wgpu renderer, with the
DXGI/libplacebo/tray modules compiled out.

## Decisions

- **Wayland.** zgui's own driver choice is kept (`App::run` → `desktop()`), so a
  Wayland session gets `zgui_platform_wayland` and anything else gets winit. X11
  is not a target but costs nothing. Do not force the backend with an extra
  `zgui-platform-wayland` dependency.
- **No tray, no notifications, no single instance.** Closing the window ends the
  process; there is no close-to-tray handler on Linux, no `release_hidden_memory`
  and no `app-single-instance`. Two UIs against one daemon are fine and useful
  while iterating.
- **The daemon is spawned if missing and left running.** `service::ensure_running`
  starts `siphon-daemon` (next to the executable, or `$SIPHON_DAEMON`), in its own
  process group so a terminal signal to the UI does not reach it. Restart is
  process-level: the PID file under `dirs::data_local_dir()/<app id>/daemon.pid`,
  verified against `/proc/<pid>/exe` before `SIGTERM`, falling back to
  `pkill -9 -x siphon-daemon`.
- **Errors are eros.** No `anyhow` anywhere in this crate. Its format flags are
  the opposite of anyhow's: `{}` is the message plus a `caused by:` block, `{:#}`
  is the message alone, `{:?}` is the full report including attached context.
  Logs want `{:?}`; strings handed to `AppError` want `{}`.
- **Commit signing.** The repo signs with the SSH key (`signing.behavior = "own"`);
  rewriting history is expected, so squashing and re-describing are cheap.

## Present path

Windows composes ZGUI's interface into a `Bgra8Unorm` texture and hands that to
libplacebo, which draws it into the DXGI backbuffers; the chain is D3D12 because
wgpu cannot hand out a swapchain the way it hands out a device. Linux does the
same thing with the swapchain Vulkan already has:

- `pl_vulkan_create_swapchain` over a `VkSurfaceKHR` built from ZGUI's
  `RawWindowHandle::Wayland` (`wl_display`, `wl_surface`). **Not** a wgpu surface:
  libplacebo drawing straight into the presented image is one copy fewer, and the
  chain is ours to shape.
- `present_mode = VK_PRESENT_MODE_MAILBOX_KHR`, `swapchain_depth = 2`.
  `swapchain_depth` counts frames in flight, not buffers — libplacebo asks for
  `depth + 1` images (`src/vulkan/swapchain.c:409`), so two in flight is three
  images: one displayed, one in the mailbox, one being drawn. With three, acquire
  always has somewhere to render, because presenting a fourth frame supersedes
  the mailbox's pending one and hands it straight back. This is the same
  arrangement as DXGI's `BufferCount: 3`.
- Probe `vkGetPhysicalDeviceSurfacePresentModesKHR` before creating the chain:
  libplacebo silently falls back to FIFO when the requested mode is unsupported
  (`swapchain.c:435-441`), and FIFO waits for the display — the one thing the UI
  thread must not do. Fall back to `FIFO_RELAXED` → `FIFO` with a warning.
- **The acquire cannot time out.** `pl_swapchain_start_frame` calls
  `vkAcquireNextImageKHR(…, UINT64_MAX, …)` (`swapchain.c:893`), so it can never
  produce `FrameOutcome::Skipped(SkipReason::Timeout)` — and that outcome is the
  only thing that arms zgui's starvation latch (pinned zgui rev,
  `zgui-runtime/src/window/frame.rs:221`), which is what handles a window the
  compositor stops drawing. So `begin()` is gated on `present_now()` and on the
  surface being visible; a hidden window parks instead of entering the driver.
  If that proves insufficient, the fix is our own `VkSwapchainKHR` with a finite
  acquire timeout — not a copy back into a wgpu surface.
- **The canvas keeps the DXGI policy** (do not "simplify" it away on Linux): the
  chain grows to the requested size immediately and only shrinks once the size
  has settled (`SETTLE`, 400 ms, no shrink while a drag is in progress). Resizing
  a swapchain reallocates its images, so following every configure of a drag would
  reallocate several times a second. The shared `present()` edge-extends the
  interface's outermost texels into the extra canvas area, which is what makes
  that invisible.

The seam is one concrete `Chain` type per platform, selected by `#[cfg]` rather
than a trait object (only one backend is ever linked, so `dyn` would buy nothing):

| method | win32 | wayland |
|---|---|---|
| `begin(ui_extent) -> Option<pl_frame>` | acquire + cached `pl_tex` + build the frame | `pl_swapchain_start_frame` |
| `end() -> bool` | `Present1` | `submit_frame` + `swap_buffers` |
| `resize(w, h) -> (w, h)` | `ResizeBuffers` + re-wrap | `pl_swapchain_resize` |
| `extent()` | the grown canvas | the same, via the resize answer |
| `can_shrink_now()` | `GetCapture() == null` | always |

Everything above it — the overlay parts, `pl_render_image`, the handover
semaphore, damage/presented gating — is shared and unchanged.

## Build

`libplacebo-sys` has no Linux story yet. It needs a `cfg(not(windows))` branch in
`build.rs`: `pkg-config` for libplacebo and the Vulkan headers, plain `cc` for the
libav glue instead of `clang-cl`, no MSVC splat and no DLL copy. `LIBPLACEBO_PREFIX`
stays as the override for a locally built prefix, on both platforms.

**The system libplacebo is too old for the hardcoded symbol.** Fedora 44 ships
7.360.1; `placebo.rs` calls `pl_log_create_372` because bindgen cannot translate
libplacebo's versioned macro and the local prefix is 7.372. Two ways out, and the
shim is the better one: add a two-line C file (`PL_LIBAV_IMPLEMENTATION` style)
that calls `pl_log_create(PL_API_VER, …)` from the installed headers and exposes
a stable symbol, so the crate builds against whatever version is present. Nothing
else the UI calls is newer than 7.360. Check that before raising the floor.

The libav glue (`pl_map_avframe_ex`, which is header-only) goes behind a `libav`
cargo feature that the video path enables, so a UI-only build needs no FFmpeg
headers.

No Linux cross-check is possible from the Windows machine: `smithay-client-toolkit`'s
build script wants `xkbcommon` for the target and `pkg-config` is not configured
for cross-compilation. A green `cargo check --target x86_64-unknown-linux-gnu` only
means the crate graph *without* native dependencies type-checks; the real build is
on the box.

## Video

Deferred, and currently blocked. `crates/video` is a spike kept as a reference for
what ZGUI can do — it is not maintained, and it is deleted once the UI plays video.

- Frame path: FFmpeg's `AVVulkanDeviceContext` is filled by hand from the wgpu-hal
  device (`crates/video/src/hw.rs`), decoded frames are mapped with
  `pl_map_avframe_ex`, and the same `pl_render_image` that draws the UI overlay
  draws the video planes. Most of that is portable as-is.
- The spike selects a decode queue that supports **HEVC only**
  (`crates/video/src/gpu.rs:185-199`). The UI must accept whatever decode codec the
  driver advertises and prefer H.264, which is what Twitch serves.
- **This box cannot decode.** Mesa 26.2.5 on the N97 (ADL-N) advertises no
  `VK_KHR_video_*` extension at all, and `MESA_VK_ENABLE_VIDEO_DECODE=1` changes
  nothing. Software decode is refused on licensing grounds. So video cannot run on
  `shivering` until the driver exposes Vulkan video decode; do not design around it
  being available.
- Transport is not settled and RTSP is probably not it: its semantics do not suit
  live monitoring. Expect a purpose-built protocol from the daemon instead. Nothing
  to build yet.

## Environment measured on `shivering`

Fedora 44, kernel 7.2.13, Intel N97 (Alder Lake-N, Mesa 26.2.5, `llvmpipe` also
present), cargo 1.100, jj installed, repo at `~/code/siphon` (jj + git colocated,
`origin` = the GitHub private repo). `libplacebo` 7.360.1 and `libavcodec` are
packaged but their `-devel` packages are not installed; bindgen also wants
`clang`/`libclang`.

Building works over plain ssh; running the window does not. The GUI needs the
desktop session's environment (`WAYLAND_DISPLAY`, `XDG_RUNTIME_DIR=/run/user/$(id -u)`)
— start the session from a terminal inside the desktop, or import those two
variables.

## Work order

1. **Bring-up** (done): portable deps moved out of `cfg(windows)`, Linux `main`
   with no tray or single instance, `service` fixed for Linux, eros throughout,
   close quits, the RPC pump wake-driven instead of a 100 ms interval, the daemon
   spawned in its own process group.
2. **Extract the chain** inside `crates/ui`, Windows behaviour byte-identical:
   `D3d12`, the `HWND`, `HdrMode`, `idle_canvas`/`resize_canvas`, the per-buffer
   `pl_tex` cache and `GetCapture` all move behind the type; `renderer.rs` and
   `app.rs` lose their `windows::` imports. Verify by running the Windows UI.
3. **libplacebo-sys Linux branch** plus the version shim; `cargo build -p siphon-ui`
   on the box.
4. **Wayland chain**: surface from the raw handles, `pl_swapchain` with the probe
   and the depth above, the canvas policy, the visibility gate. Verify with
   `cargo run -p siphon-ui` in the desktop session and a live channel.
5. **Video**, when the hardware or the driver allows it.

Steps 2 and 4 are separable on purpose: the extraction has to leave Windows
provably unchanged, and the Linux chain is then one new file behind the same seam.
