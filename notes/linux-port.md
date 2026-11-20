# Linux port

Linux is where the UI gets iterated on away from the Windows desktop: same ZGUI
view, same daemon, same RPC. Nothing here changes what Windows does — the DXGI
path stays the reference implementation and every commit must leave it running
identically.

The port is done except video: `cargo run -p siphon` opens the same ZGUI view on
Wayland, presenting through libplacebo's own `VkSwapchainKHR` on ZGUI's surface.
The Windows DXGI chain was extracted behind the same seam and behaves as before.
Only the tray, single-instance handover and Win32 shell remain Windows-only.

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
- `present_mode = VK_PRESENT_MODE_MAILBOX_KHR`, `swapchain_depth = 2`. wgpu
  enables `VK_KHR_wayland_surface` on its instance and requires
  `VK_KHR_swapchain` on its device, so the surface and libplacebo's chain can
  both be created on the handles wgpu already owns.
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
  `zgui-runtime/src/window/frame.rs:221`). The Wayland chain therefore gates every
  acquire on `Surface::present_now()`: when the compositor has not delivered a
  frame callback, the frame is dropped and the damage flag is kept. That also
  keeps libplacebo's `wl_surface_commit` in step with zgui's frame pacer. If a
  hidden window still parks inside the driver, the fix is our own
  `VkSwapchainKHR` with a finite acquire timeout — not a copy back into a wgpu
  surface.
- **The canvas policy is Windows-only.** Windows grows to the requested size
  immediately and shrinks once it settles (`SETTLE`, 400 ms, no shrink during a
  drag), with the shared `present()` edge-extending the interface's outermost
  texels into the extra area. Wayland follows the surface extent exactly: Mesa's
  WSI scales an image that differs from the surface instead of cropping it, so a
  larger canvas would scale the whole interface down rather than show its
  top-left corner 1:1. `pl_swapchain_resize` writes back the extent it actually
  created, which is the one used for the overlay parts. `Chain::configure`
  returns that granted extent and the renderer rebuilds the UI for it: during a
  maximize the configure can arrive before the surface grows, and rebuilding
  for the requested extent instead made a large interface render into the old
  small surface (the window looked shrunken and the content tiny until a later
  configure arrived). A second half of the bug was window placement: KWin
  decides a maximize's final frame position from the window geometry declared
  when its animation starts, and a stale 1000-wide declaration made it place
  the 1920-wide frame at x=-460 (half the size change off-screen). The renderer
  now calls `Chain::declare` on configure, with the extent the compositor just
  granted, before the swapchain is rebuilt, and `end` declares the frame's
  actual extent rather than the surface's possibly stale one. Maximize lands at
  the origin and restore returns to the old geometry.

## Display settings

Two presentation choices belong to the client rather than the daemon, so they
live in `crates/ui/src/settings.rs` over `siphon_shared::settings::UiSettings`: a
`ui.json` beside the daemon's `config.json`, because a display belongs to the
screen a window is on and a second UI — or a remote one — must be able to differ.

- **HDR.** `HdrMode::Off | Auto | On` is what `configure_hdr` asks DXGI for;
  `SIPHON_HDR=off|auto|on` overrides it for a session. When the chain actually
  ends up in PQ/Rec.2020, `target_color` declares the target as BT.2020/PQ with
  the display's reported peak, so libplacebo encodes into the container instead
  of writing the SDR transfer into it. The interface overlay stays declared SDR
  with `max_luma = sdr_white`: it *is* SDR content, and mapping it into the
  container is libplacebo's job. With `Auto` the chain follows the output: the
  present path re-reads the display twice a second (and on `WM_DISPLAYCHANGE`),
  so a window on a display switched to HDR re-applies the colour space in place.
  The *mode* is fixed at chain creation, so a different `Off`/`Auto`/`On` is a
  new chain: `Chain::apply_display` runs from `draw`, before the frame is
  acquired. The default is `Auto`; `Off` is the switch's answer for a display
  left in HDR that something cannot capture from.
- **Tearing.** The switch asks for an unpaced present —
  `Present1(0, DXGI_PRESENT_ALLOW_TEARING)` when the adapter reports
  `DXGI_FEATURE_PRESENT_ALLOW_TEARING`, `Present1(0, …)` when it does not (still
  latest-frame-wins; nothing can tear), and `Present1(1, …)` when off. The
  feature has to be checked before the chain exists: the flag in
  `DXGI_SWAP_CHAIN_DESC1` makes `CreateSwapChainForHwnd` fail outright on an
  adapter without it, so it is queried once and the flags kept for
  `ResizeBuffers` to be given back unchanged.

Wayland ignores both. A `VkSwapchainKHR` takes its colour space from the
compositor's image description and has no tearing mode — mailbox, chosen at
creation, is already the unpaced path. The settings are still read and saved so
one client file moves between platforms intact.

The Windows half of both is **type-checked only**; it has not been run on
hardware, so the HDR target colour, the SDR white point inside an HDR container
and the tear behaviour all still want a real display.

## The marquee jitters under the pinned zgui

Known regression, not chased to a commit: scrolling text in the header ticker
shimmers along its edges as it moves, where it did not before `2a5dcfd` moved the
zgui pin from `80baa3c7` to `aacaa469`.

It is not the subpixel-coverage work that landed in between. On this display
ZGUI still draws *one coverage for each pixel*, and a capture says so directly:
of the edge pixels in a static text run, 94% carry the text's own hue and 6% the
other side, where per-channel coverage would split them roughly evenly and tint
one side of every stem red and the other blue. It is not the present path either
— `Placebo::render` is untouched,
and everything the merge added around it (`request_frame`, `SIPHON_PLAIN`, the
diagnostics) is inert on Wayland. What is left is how the pinned ZGUI composes a
translate-animated text run inside the interface texture: the ticker is a CSS
`translateX` over eight copies, so every frame asks for glyphs at a new
fractional phase.

To pin it down, bisect the pin: `f38307f` is the tree just before
partial-present (`#72`) and `80baa3c7` is the pre-pull revision, so a build at
each says which side of that merge owns it. A workaround that does not depend on
the answer is to drive the marquee from the frame callback in whole device
pixels instead of animating a transform.

## Why the switch did not slide

The switch's `:checked` transition is fine: ZGUI's harness interpolates it
frame by frame with the app's exact selector structure (state on the parent,
`transform` overridden by a second sheet), and instrumenting the runtime showed
the thumb's matrix moving 14→12→…→0 in even steps. The fault was the present
path starving the animation of frames.

`Chain::wait_for_frame` called `Surface::present_now()`. Besides answering
whether a frame callback is owed, that call **marks a frame as held in ZGUI's
surface pacer** — state only a renderer that finishes held frames through
`Renderer::present_held()` can release. This renderer presents immediately and
never completes a held frame. Worse, libplacebo's swapchain uses Vulkan WSI,
which issues its own `wl_surface.frame()` request alongside ZGUI's; the
callback ZGUI waited for therefore arrived one serial behind the one its
200 ms watchdog probe had already asked for, was discarded, and left the pacer
permanently owed. Measured: 60 redraw requests but 30 frames in 97 s, presents
exactly 205 ms apart (the watchdog floor), every other frame dropped. A 160 ms
transition completed between two presents and only its endpoints were visible.

The fix has two halves: `Chain::wait_for_frame` no longer calls `present_now()`
on Wayland (the mailbox swapchain paces itself), and the window is opened with
`ComposeAhead::Always` so ZGUI keeps composing at the animation cadence instead
of waiting on a callback the swapchain will not let it accept. Measured after:
1534 frames in 26 s (58 fps), 16.7 ms apart, all presented. The renderer's
local `damaged`/`presented` gate was also removed — it could drop an
animation-only frame with no layout damage — but that alone was not the cause.

The seam is one concrete `Chain` type per platform, selected by `#[cfg]` rather
than a trait object (only one backend is ever linked, so `dyn` would buy nothing):

| method | win32 | wayland |
|---|---|---|
| `configure(ui)` | monitor-sized growth on overflow + settle timer | `pl_swapchain_resize` to `ui` |
| `settle(ui)` | shrink after `SETTLE` when `GetCapture() == null` | no-op |
| `wait_for_frame(ms)` | frame-latency waitable object | no-op (mailbox swapchain paces itself) |
| `begin(ui) -> Option<Acquired>` | current backbuffer + cached `pl_tex` + build `pl_frame` | `pl_swapchain_start_frame` + `pl_frame_from_swapchain` |
| `end() -> bool` | `Present1` | `submit_frame` + `swap_buffers` |

Everything above it — the overlay parts, `pl_render_image`, the handover
semaphore — is shared and unchanged. The chain owns its per-buffer textures and
destroys them before its swapchain.

## Build

`libplacebo-sys` builds on both platforms. Windows keeps its prefix knobs;
Linux uses `pkg-config` for libplacebo and the Vulkan headers, plain `cc` for the
C glue, and no DLL copy. `LIBPLACEBO_PREFIX` wins when set. Fedora's libclang
derives its resource dir from `libclang.so.22.1` and misses `clang/22`, so the
build script asks `clang -print-resource-dir` for `stdbool.h`'s directory.

The version problem is solved by `src/shim.c`: libplacebo's `pl_log_create` is a
macro expanding to `pl_log_create_<PL_API_VER>`, so callers used to hardcode
`_372` and could not link Fedora's 7.360. The shim expands the macro against the
headers it compiles with and exports one `siphon_pl_log_create`. Fields added
after 7.360 (for example `pl_vulkan_queue.flags`) are handled with
`..Default::default()` in struct literals so one source builds against both.
The header-only libav glue sits behind the `libav` feature, which only the video
path enables; the UI build needs no FFmpeg headers.

No Linux cross-check is possible from the Windows machine (its build scripts want
`xkbcommon` for the target). The other direction works with one caveat: installing
`x86_64-pc-windows-gnu` and running `cargo check --target x86_64-pc-windows-gnu
-p siphon-ui` type-checks the Windows-only chain without linking, but
`libplacebo-sys` then wants the Windows prefixes (`C:/Users/alex/...`) that do
not exist here. Point `paths` at a tiny stub crate named `libplacebo-sys`
containing the already-generated Linux `bindings.rs` plus the shim declaration
to get past that. This is a type-check, not a runtime verification — the Windows
UI still has to be run on the Windows machine.

## Video

Deferred in the UI, but the backend question is settled: **VA-API on Linux,
D3D12 Video on Windows**, with libplacebo importing the decoded frames without a
copy. The full plan, measurements, the driver trap and why the UI links libav
dynamically are in `notes/video-decode.md`. `crates/video` is a spike kept as a
reference for what ZGUI can do — it is not maintained, and it is deleted once
the UI plays video.

- Frame path: FFmpeg's `AVVulkanDeviceContext` is filled by hand from the wgpu-hal
  device (`crates/video/src/hw.rs`), decoded frames are mapped with
  `pl_map_avframe_ex`, and the same `pl_render_image` that draws the UI overlay
  draws the video planes. Most of that is portable as-is; only the decode backend
  changes.
- The spike's Vulkan decode queue selection (`crates/video/src/gpu.rs:185-199`,
  HEVC only) goes with it: Mesa 26.2.6 on the N97 (ADL-N) advertises no Vulkan
  video codec extension at all, so there is nothing to select.
- **This box does decode — Fedora's driver build does not.** Fedora ships the
  free `libva-intel-media-driver`, whose H.264/HEVC/AV1 decoders are compiled out;
  `intel-media-driver` from RPM Fusion restores them. Software decode stays
  refused on licensing grounds, so a missing hardware profile means no video.
- Transport is a purpose-built elementary-stream channel on the admin RPC, not
  RTSP; the design is in `notes/media-transport.md`. Nothing is built yet.

## Environment measured on `shivering`

Fedora 44, kernel 7.2.13, Intel N97 (Alder Lake-N, Mesa 26.2.6, `llvmpipe` also
present), cargo 1.100, jj installed, repo at `~/code/siphon` (jj + git colocated,
`origin` = the GitHub private repo). `libplacebo` 7.360.1 and `libavcodec` are
installed with their `-devel` packages, and `intel-media-driver` (RPM Fusion) is
installed next to Fedora's free `libva-intel-media-driver` — see
`notes/video-decode.md` for why both are listed.

Building works over plain ssh; running the window needs the desktop session's
environment: `WAYLAND_DISPLAY=wayland-1`, `XDG_RUNTIME_DIR=/run/user/$(id -u)`
and `DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/$(id -u)/bus`. With those set,
`./target/debug/siphon` opens the window and spawns `siphon-daemon` next to the
binary if the admin port is idle.

## Work order

1. **Bring-up** (done): portable deps moved out of `cfg(windows)`, Linux `main`
   with no tray or single instance, `service` fixed for Linux, eros throughout,
   close quits, the RPC pump wake-driven instead of a 100 ms interval, the daemon
   spawned in its own process group.
2. **Extract the chain** (done): `crates/ui/src/chain/{windows,wayland}.rs`. The
   Windows chain owns `D3d12`, the `HWND`, `HdrMode`, the monitor canvas, the
   per-buffer `pl_tex` cache and `GetCapture`; `renderer.rs` and `app.rs` have no
   `windows::` imports. The Windows cross-target check type-checks it; running
   the Windows UI is still the final verification.
3. **libplacebo-sys Linux branch** (done): pkg-config, the `pl_log_create` shim,
   the `libav` feature. `cargo build -p siphon-ui` is green on the box.
4. **Wayland chain** (done): surface from ZGUI's raw handles, present-mode probe,
   depth 2, `present_now()` gate. Verified in the live session: the window draws
   its full interface through libplacebo and survives resizes without artefacts.
5. **Video** (deferred): VA-API decode behind the `decode` seam, per
   `notes/video-decode.md`. The renderer chain is now in place; video is the only
   thing left.
