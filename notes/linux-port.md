# Linux port

The port is done except video. Linux is where the UI is iterated on away from the
Windows desktop: same ZGUI view, same daemon, same RPC. Nothing here changes what
Windows does — the DXGI chain stays the reference implementation and every commit
must leave it running identically. Only the tray, single-instance handover and the
Win32 shell remain Windows-only.

## Decisions

- **Wayland.** zgui's own driver choice is kept (`App::run` → `desktop()`): a
  Wayland session gets `zgui_platform_wayland`, anything else winit. X11 is not a
  target but costs nothing. Do not force the backend with an extra
  `zgui-platform-wayland` dependency.
- **No tray, no notifications, no single instance.** Closing the window ends the
  process; two UIs against one daemon are fine while iterating.
- **The daemon is spawned if missing and left running.** `service::ensure_running`
  starts `siphon-daemon` (next to the executable, or `$SIPHON_DAEMON`) in its own
  process group. Restart is process-level: the PID file under
  `dirs::data_local_dir()/<app id>/daemon.pid`, verified against `/proc/<pid>/exe`
  before `SIGTERM`, falling back to `pkill -9 -x siphon-daemon`.
- **Errors are eros.** No `anyhow` in the UI crate. `{}` is the message plus the
  `caused by:` block, `{:#}` the message alone, `{:?}` the full report: logs want
  `{:?}`, strings handed to `AppError` want `{}`.

## Present path

Windows composes ZGUI's interface into a `Bgra8Unorm` texture and hands it to
libplacebo, which draws it into DXGI backbuffers. Linux does the same with the
swapchain Vulkan already has:

- `pl_vulkan_create_swapchain` over a `VkSurfaceKHR` built from ZGUI's
  `RawWindowHandle::Wayland` (`wl_display`, `wl_surface`). **Not** a wgpu surface:
  libplacebo drawing straight into the presented image is one copy fewer, and the
  chain stays ours to shape.
- `present_mode = VK_PRESENT_MODE_MAILBOX_KHR`, `swapchain_depth = 2`.
  `swapchain_depth` counts frames in flight, not buffers — libplacebo asks for
  `depth + 1` images (`src/vulkan/swapchain.c:409`), so two in flight is three
  images: one displayed, one in the mailbox, one being drawn. Presenting a fourth
  supersedes the mailbox's pending image and hands it straight back. Same
  arrangement as DXGI's `BufferCount: 3`.
- Probe `vkGetPhysicalDeviceSurfacePresentModesKHR` before creating the chain:
  libplacebo silently falls back to FIFO when the requested mode is unsupported
  (`swapchain.c:435-441`), and FIFO waits for the display, which the UI thread
  must never do. Fall back `MAILBOX` → `FIFO_RELAXED` → `FIFO` with a warning.
- **The acquire cannot time out.** `pl_swapchain_start_frame` calls
  `vkAcquireNextImageKHR(…, UINT64_MAX, …)` (`swapchain.c:893`), and the timeout
  outcome is the only thing that arms zgui's starvation latch. The Wayland chain
  therefore drops a frame whenever `Surface::present_now()` says the compositor
  has not delivered a frame callback, keeping the damage flag. If a hidden window
  still parks inside the driver, the fix is our own `VkSwapchainKHR` with a finite
  acquire timeout — not a copy back into a wgpu surface.
- **The canvas policy is Windows-only.** Windows grows to the requested size
  immediately and shrinks once it settles (`SETTLE`, 400 ms, never during a drag),
  edge-extending the interface's outermost texels into the extra area. Wayland
  follows the surface extent exactly: Mesa's WSI *scales* an image that differs
  from the surface instead of cropping it, so a larger canvas would scale the
  whole interface down. `pl_swapchain_resize` writes back the granted extent and
  `Chain::configure` returns it, so the renderer rebuilds the UI for what was
  actually granted. The renderer calls `Chain::declare` on configure with that
  extent, and `end` declares the frame's actual extent: KWin derives a maximize's
  final position from the geometry declared when its animation starts, so a stale
  declaration puts the frame off-screen.

## Display settings

Both presentation choices belong to the client, so they live in
`crates/ui/src/settings.rs` over `siphon_shared::settings::UiSettings`, in `ui.json`
beside the daemon's `config.json`: a display belongs to the screen a window is on,
and a second UI must be able to differ.

- **HDR.** `HdrMode::Off | Auto | On` is what `configure_hdr` asks DXGI for;
  `SIPHON_HDR=off|auto|on` overrides it for a session. When the chain actually
  ends up in PQ/Rec.2020, `target_color` declares BT.2020/PQ at the display's
  reported peak, so libplacebo encodes into the container instead of writing the
  SDR transfer into it. The interface overlay stays declared SDR with
  `max_luma = sdr_white`: it *is* SDR content, and mapping it into the container
  is libplacebo's job. With `Auto` the present path re-reads the display twice a
  second (and on `WM_DISPLAYCHANGE`). The *mode* is fixed at chain creation, so a
  different `Off`/`Auto`/`On` is a new chain: `Chain::apply_display` runs from
  `draw`, before the frame is acquired. Default `Auto`.
- **Tearing.** `Present1(0, DXGI_PRESENT_ALLOW_TEARING)` when the adapter reports
  the feature, `Present1(0, …)` when it does not (still latest-frame-wins), and
  `Present1(1, …)` when off. The feature must be queried before the chain exists:
  the desc flag makes `CreateSwapChainForHwnd` fail outright without it, and the
  flags are kept for `ResizeBuffers` to be given back unchanged.

Wayland ignores both. A `VkSwapchainKHR` takes its color space from the
compositor's image description and has no tearing mode; mailbox, chosen at
creation, is already the unpaced path. Both settings are still read and saved so
one client file moves between platforms intact. The Windows half is **type-checked
only** — it has not run on hardware, so the HDR target color, the SDR white point
inside an HDR container and the tear behavior still want a real display.

## The seam

One concrete `Chain` per platform, selected by `#[cfg]` rather than a trait object
(only one backend is ever linked):

| method | win32 | wayland |
|---|---|---|
| `configure(ui)` | monitor-sized growth on overflow + settle timer | `pl_swapchain_resize` to `ui` |
| `settle(ui)` | shrink after `SETTLE` when `GetCapture() == null` | no-op |
| `wait_for_frame(ms)` | frame-latency waitable object | no-op (mailbox swapchain paces itself) |
| `begin(ui) -> Option<Acquired>` | current backbuffer + cached `pl_tex` + build `pl_frame` | `pl_swapchain_start_frame` + `pl_frame_from_swapchain` |
| `end() -> bool` | `Present1` | `submit_frame` + `swap_buffers` |

Everything above it — the overlay parts, `pl_render_image`, the handover
semaphore — is shared. The chain owns its per-buffer textures and destroys them
before its swapchain.

## Build

`libplacebo-sys` builds on both platforms, and `system-deps` owns discovery:
`[package.metadata.system-deps]` names libplacebo, Vulkan (optional, for the
headers) and, behind the `libav` feature, the FFmpeg libraries. Both platforms
use pkg-config; the Windows install prefix's `.pc` lives in the machine's
pkgconf path (this checkout's is in
`scoop/persist/pkgconf/lib/pkgconfig`), and `LIBPLACEBO_PREFIX` names a prefix
explicitly instead. The checkout itself is
`~/Projects/Code/Libs/libplacebo`, named to a source build through
`LIBPLACEBO_SRC`. The checkout's Windows `.venv` holds meson, ninja and
jinja2; the build script puts its `Scripts` on `PATH` for the meson run,
because the uv-managed Python on this machine has no jinja2.

`LIBPLACEBO_SOURCE` picks the origin on either platform: `system` (the default),
`prefix` (a `LIBPLACEBO_PREFIX`), `build` (meson compiles
`LIBPLACEBO_SRC` into `OUT_DIR` or `LIBPLACEBO_BUILD_DIR`) or `auto` (system,
then build). `LIBPLACEBO_LINK=static` links the source build statically;
dynamic is the default. A source build forwards its libdir to the UI's build
script: that adds the link search path (a dependency's `/usr/lib64 -L` can
otherwise shadow it) and, for a shared build, an rpath, so
`./target/debug/siphon` runs without `LD_LIBRARY_PATH`.

Fedora's libclang derives its resource dir from `libclang.so.22.1` and misses
`clang/22`, so the build script asks `clang -print-resource-dir` for
`stdbool.h`'s directory.

`src/shim.c` solves the version problem: libplacebo's `pl_log_create` is a macro
expanding to `pl_log_create_<PL_API_VER>`, so the shim expands it against the
headers it compiles with and exports one `siphon_pl_log_create`. Fields added after
7.360 (`pl_vulkan_queue.flags`) are handled with `..Default::default()` so one
source builds against both. The header-only libav glue sits behind the `libav`
feature, which only the video path enables; the UI build needs no FFmpeg headers.

A Windows cross-check of the UI needs a Windows libplacebo, not a Linux one.
`nix develop .#windows` provides it: `nix/msvc.nix` cross-builds libplacebo (and
the static shaderc it links, so the prefix can compile shaders rather than only
type-check) against the pinned CRT/SDK, and the shell exports
`LIBPLACEBO_PREFIX`/`VULKAN_SDK`/`CARGO_BUILD_TARGET`:

    nix develop .#windows -c cargo clippy -p siphon-ui --all-targets

Cargo has no per-target `env` table and exporting `LIBPLACEBO_PREFIX` globally
would send native builds at a Windows library, which is why that environment
lives in a separate shell rather than the default one. `libav` stays off, so no
Windows FFmpeg is needed; the build script only asks for `FFMPEG_DIR` when the
feature is on.

## Video

Deferred. The backend is settled: **VA-API on Linux, D3D12 Video on Windows**, with
libplacebo importing decoded frames without a copy — `notes/video-decode.md` has
the plan, the measurements and the driver trap. Transport is Riffle's media lane
over the admin connection (`notes/riffle.md`): the UI reassembles whole access
units, but nothing decodes them yet.

## Environment

Fedora 44, kernel 7.2.13, Intel N97 (Alder Lake-N, Mesa 26.2.6, `llvmpipe` also
present), cargo 1.100, jj, repo at `~/code/siphon` (jj + git colocated, `origin` =
the GitHub private repo). `libplacebo` 7.360.1 and `libavcodec` are installed with
their `-devel` packages, and `intel-media-driver` (RPM Fusion) sits next to
Fedora's free `libva-intel-media-driver` — see `notes/video-decode.md` for why both
are listed.

Building works over plain ssh; running the window needs the desktop session's
environment: `WAYLAND_DISPLAY=wayland-1`, `XDG_RUNTIME_DIR=/run/user/$(id -u)` and
`DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/$(id -u)/bus`. With those set,
`./target/debug/siphon` opens the window and spawns `siphon-daemon` next to the
binary if the admin port is idle.
