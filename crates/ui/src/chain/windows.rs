//! The Windows chain: a D3D12 flip chain whose backbuffers are Vulkan images.
//!
//! ZGUI composes the interface into a wgpu texture; this chain draws that
//! texture over the diagnostics fill, straight into the current backbuffer,
//! and presents. The GPU work is handed to D3D12 through the chain's timeline
//! semaphore, so `Present` is queued behind a wait rather than the CPU.
//!
//! `SIPHON_SYNC` selects which of the three edges are synchronised on the CPU.
//! It exists because the point of this build is to find which edge the banding
//! comes from, and a hard-coded sync would remove an edge from suspicion:
//!
//! - `compose` — wgpu's compose is finished before the pass samples it (RAW).
//! - `handover` — last frame's pass is finished before wgpu writes the interface
//!   texture again (WAR).
//! - `present` — the frame is finished before `Present` hands it over.
//!
//! Comma separated, `all`, `none`, or unset (none).

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use ash::vk;
use eros::Context as _;
use raw_window_handle::HasWindowHandle as _;
use tracing::warn;
use windows::Win32::Foundation::HWND;
use windows::Win32::Foundation::LPARAM;
use windows::Win32::Foundation::LRESULT;
use windows::Win32::Foundation::WPARAM;
use windows::Win32::Graphics::Gdi::InvalidateRect;
use windows::Win32::UI::Shell::DefSubclassProc;
use windows::Win32::UI::Shell::RemoveWindowSubclass;
use windows::Win32::UI::Shell::SetWindowSubclass;
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;
use windows::Win32::UI::WindowsAndMessaging::WM_DISPLAYCHANGE;
use windows::Win32::UI::WindowsAndMessaging::WM_ENTERSIZEMOVE;
use windows::Win32::UI::WindowsAndMessaging::WM_EXITSIZEMOVE;
use windows::Win32::UI::WindowsAndMessaging::WM_MOUSEMOVE;
use windows::Win32::UI::WindowsAndMessaging::WM_MOVING;
use windows::Win32::UI::WindowsAndMessaging::WM_PAINT;
use windows::Win32::UI::WindowsAndMessaging::WM_SIZE;
use zgui::platform::GpuSurface;
use zgui::platform::Surface;
use zgui_render_wgpu::Gpu;
use zgui_render_wgpu::wgpu;

use super::Canvas;
use super::blit::Blit;
use super::idle_canvas;
use super::pass::Pass;
use crate::dxgi::BUFFERS;
use crate::dxgi::D3d12;
use crate::dxgi::DxgiSwapchain;
use crate::dxgi::HdrMode;
use crate::dxgi::hwnd_from_raw;
use crate::dxgi::resize_canvas;
use crate::dxgi::set_window_background;

/// The presentation mode to ask DXGI for: `SIPHON_HDR=off|auto|on`.
fn hdr_mode() -> HdrMode {
    match std::env::var("SIPHON_HDR").as_deref() {
        Ok(value) if value.eq_ignore_ascii_case("off") => HdrMode::Off,
        Ok(value) if value.eq_ignore_ascii_case("on") => HdrMode::On,
        _ => HdrMode::Auto,
    }
}

/// Which edges are synchronised on the CPU. See the module docs.
#[derive(Clone, Copy, Debug, Default)]
struct Sync {
    compose: bool,
    handover: bool,
    present: bool,
}

impl Sync {
    /// Reads `SIPHON_SYNC`. An unknown name is fatal: a typo must not quietly
    /// test the wrong edge.
    fn from_env() -> Self {
        let Ok(value) = std::env::var("SIPHON_SYNC") else {
            return Self::default();
        };
        let mut sync = Self::default();
        for name in value.split(',').map(str::trim).filter(|it| !it.is_empty()) {
            match name {
                "all" => {
                    sync = Self {
                        compose: true,
                        handover: true,
                        present: true,
                    };
                }
                "none" => sync = Self::default(),
                "compose" => sync.compose = true,
                "handover" => sync.handover = true,
                "present" => sync.present = true,
                other => panic!("SIPHON_SYNC: unknown edge {other:?}"),
            }
        }
        eprintln!("sync: {sync:?}");
        sync
    }
}

/// The D3D12 device, the swapchain and the pass that fills its backbuffers.
pub struct Chain {
    d3d12: Rc<D3d12>,
    gpu: Arc<Gpu>,
    hwnd: HWND,
    swapchain: Option<DxgiSwapchain>,
    pass: Option<Pass>,
    canvas: Canvas,
    /// The display state the pass grades against. Cells because a refresh runs
    /// under `&self` on the present path.
    sdr_white: Cell<f32>,
    hdr: Cell<bool>,
    peak: Cell<f32>,
    /// When the display was last re-read; the slider in Windows Settings moves
    /// without a resize, so the chain polls it a few times a second.
    refreshed: Cell<Option<Instant>>,
    /// The backbuffer the current frame acquired, and the timeline values that
    /// hand the frame between Vulkan, D3D12 and the compositor.
    acquired: Cell<usize>,
    submit: Cell<u64>,
    /// Per canvas, the value its last copy signalled: the point after which
    /// that canvas is free for the next render. There is one canvas per
    /// backbuffer, so this is indexed by the backbuffer.
    blit_value: Cell<[u64; super::blit::CANVASES]>,
    blit: Option<Blit>,
    sync: Sync,
    /// One command buffer per backbuffer: DXGI only hands a buffer out again
    /// once its frame has retired, which is also when its commands are done.
    pool: vk::CommandPool,
    commands: Vec<vk::CommandBuffer>,
    queue: vk::Queue,
    device: ash::Device,
}

/// How often the display state is re-read on the present path.
const REFRESH: Duration = Duration::from_millis(500);

/// The subclass slot this chain owns on the window.
const SUBCLASS_ID: usize = 0x5150;

/// Frames the present path has actually begun, for the drag diagnostic.
static PRESENTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Paints the window procedure delivered, and invalidations we asked for.
static PAINTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static INVALIDATES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Records that the present path ran.
pub fn note_present() {
    PRESENTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// Whether the shell owns the window in a modal move/resize loop.
static IN_MOVE_LOOP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether the shell is running its move/resize loop right now.
pub fn in_move_loop() -> bool {
    IN_MOVE_LOOP.load(std::sync::atomic::Ordering::Relaxed)
}

/// Posted to ask for another frame once the current paint has been handled.
const NUDGE: u32 = windows::Win32::UI::WindowsAndMessaging::WM_APP + 1;

/// Set by the window subclass when Windows broadcasts `WM_DISPLAYCHANGE`.
static DISPLAY_CHANGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Marks the display state stale and lets every other message through.
unsafe extern "system" fn display_change_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    match message {
        WM_DISPLAYCHANGE => DISPLAY_CHANGED.store(true, std::sync::atomic::Ordering::Relaxed),
        WM_ENTERSIZEMOVE => IN_MOVE_LOOP.store(true, std::sync::atomic::Ordering::Relaxed),
        WM_EXITSIZEMOVE => IN_MOVE_LOOP.store(false, std::sync::atomic::Ordering::Relaxed),
        NUDGE => {
            INVALIDATES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            // SAFETY: `hwnd` is this subclass's window.
            let _ = unsafe { InvalidateRect(Some(hwnd), None, false) };
        }
        WM_PAINT => {
            PAINTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        WM_MOUSEMOVE | WM_MOVING | WM_SIZE => {
            // A dragged window is not damaged: its content travels with it, so
            // nothing asks ZGUI for a frame. Mark it damaged at the rate the
            // shell is moving it at.
            // SAFETY: `hwnd` is the window this subclass belongs to.
            let _ = unsafe { InvalidateRect(Some(hwnd), None, false) };
            // Rate limited: this is only here to show whether the window is
            // still being pumped while the shell owns the drag.
            thread_local! {
                static LAST: Cell<Option<Instant>> = const { Cell::new(None) };
            }
            let now = Instant::now();
            LAST.with(|last| {
                if last
                    .get()
                    .is_none_or(|at| now.duration_since(at).as_millis() >= 500)
                {
                    last.set(Some(now));
                    eprintln!(
                        "wndproc: msg={message} drag={} presents={}",
                        IN_MOVE_LOOP.load(std::sync::atomic::Ordering::Relaxed),
                        PRESENTS.load(std::sync::atomic::Ordering::Relaxed),
                    );
                }
            });
        }
        _ => {}
    }
    // SAFETY: chained to the next procedure in the subclass chain, which owns
    // the window and the rest of its messages.
    unsafe { DefSubclassProc(hwnd, message, wparam, lparam) }
}

impl Chain {
    pub fn new(
        gpu: &Arc<Gpu>,
        _surface: &Arc<dyn Surface>,
        gpu_surface: &Arc<dyn GpuSurface>,
        width: u32,
        height: u32,
    ) -> eros::Result<Self> {
        let window = gpu_surface
            .window_handle()
            .map_err(|error| eros::error!("window handle: {error}"))?;
        let hwnd = hwnd_from_raw(window.as_raw())?;
        // A display mode change (HDR on/off, resolution) reaches us as
        // `WM_DISPLAYCHANGE`; the SDR brightness slider does not, so the chain
        // polls that. The two together are what keeps the presentation live.
        // SAFETY: `hwnd` is this window and the procedure is a plain `extern
        // "system"` fn; the id is unique to this chain.
        let _ = unsafe { SetWindowSubclass(hwnd, Some(display_change_proc), SUBCLASS_ID, 0) };
        set_window_background(hwnd);
        // The icon helper speaks `windows-sys`, whose `HWND` is the raw handle;
        // `hwnd` is the `windows` crate's newtype around the same pointer.
        crate::tray::set_window_icon(hwnd.0);

        let d3d12 = Rc::new(D3d12::new(gpu).context("D3D12")?);

        // Everything that needs the hal guards is done in one scope: `rebuild`
        // below takes the same snatch lock, and wgpu aborts on re-entry.
        let device;
        let queue;
        let pool;
        let commands;
        {
            // SAFETY: `as_hal` only asks that the resource behind the guard
            // outlives the borrow. Both wgpu handles are alive for the scope.
            let hal_adapter = unsafe { gpu.adapter().as_hal::<wgpu::hal::api::Vulkan>() }
                .context("the video device is not Vulkan")?;
            // SAFETY: as above.
            let hal_device = unsafe { gpu.device().as_hal::<wgpu::hal::api::Vulkan>() }
                .context("the video device is not Vulkan")?;
            let instance = hal_adapter.shared_instance().raw_instance();
            // SAFETY: the handle is the device wgpu created from this instance.
            device = unsafe {
                ash::Device::load(instance.fp_v1_0(), hal_device.raw_device().handle())
            };
            // SAFETY: the family belongs to this device.
            queue = unsafe {
                hal_device
                    .raw_device()
                    .get_device_queue(hal_device.queue_family_index(), 0)
            };
            // SAFETY: the pool is created on the device's own graphics family
            // and freed in `Drop`.
            pool = unsafe {
                device.create_command_pool(
                    &vk::CommandPoolCreateInfo::default()
                        .queue_family_index(hal_device.queue_family_index())
                        .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER),
                    None,
                )
            }
            .context("vkCreateCommandPool")?;
            // SAFETY: the pool is live and has room for the chain's buffers.
            commands = unsafe {
                device.allocate_command_buffers(
                    &vk::CommandBufferAllocateInfo::default()
                        .command_pool(pool)
                        .level(vk::CommandBufferLevel::PRIMARY)
                        .command_buffer_count(BUFFERS),
                )
            }
            .context("vkAllocateCommandBuffers")?;
        }

        let mut this = Self {
            d3d12,
            gpu: Arc::clone(gpu),
            hwnd,
            swapchain: None,
            pass: None,
            canvas: Canvas::new(width, height),
            sdr_white: Cell::new(200.0),
            hdr: Cell::new(false),
            peak: Cell::new(0.0),
            refreshed: Cell::new(None),
            acquired: Cell::new(0),
            submit: Cell::new(0),
            blit_value: Cell::new([0; super::blit::CANVASES]),
            blit: None,
            sync: Sync::from_env(),
            pool,
            commands,
            queue,
            device,
        };
        this.rebuild(width, height)?;
        Ok(this)
    }

    /// Resizes the chain and rebuilds the pass's views, or creates it the first
    /// time. Nothing else may be in flight over the old images: the device is
    /// waited for before `ResizeBuffers` runs.
    fn rebuild(&mut self, width: u32, height: u32) -> eros::Result<()> {
        // The canvases and their framebuffers are thrown away below, and the
        // D3D12 queue may still hold a copy of one. Without this the queue
        // wedges on a resource that no longer exists, and the flip queue stops
        // retiring for good: the window keeps working at 10 fps (the waitable
        // timing out) and never recovers.
        let t_start = Instant::now();
        if let Some(swapchain) = self.swapchain.as_ref() {
            swapchain.drain();
        }
        let t_drain = t_start.elapsed();
        // SAFETY: the device is live; this waits for every queue, which is what
        // dropping the images and framebuffers over them requires.
        unsafe { self.device.device_wait_idle() }.context("vkDeviceWaitIdle")?;
        let t_idle = t_start.elapsed();

        // The chain is reused through `ResizeBuffers`; creating a new one would
        // make the window association and the flip queue start over. A chain
        // that cannot be resized is dropped, and the next attempt creates one.
        let resized = match self.swapchain.as_mut() {
            Some(existing) => existing.resize(&self.d3d12, &self.gpu, width, height),
            None => DxgiSwapchain::new(
                &self.d3d12,
                &self.gpu,
                self.hwnd,
                width,
                height,
                hdr_mode(),
            )
            .map(|fresh| {
                self.swapchain = Some(fresh);
            }),
        };
        if let Err(error) = resized {
            warn!(target: "ui", "DXGI: resize failed: {error:?}");
            return Err(error);
        }
        let Some(swapchain) = self.swapchain.as_ref() else {
            return Err(eros::error!("the swapchain was not created"));
        };
        let t_resize = t_start.elapsed();
        // The canvas D3D12 owns and Vulkan renders into, and the list that
        // copies it into whichever backbuffer comes next.
        let blit = Blit::new(&self.d3d12, &self.gpu, width, height)?;
        let t_blit = t_start.elapsed();
        let images: Vec<vk::Image> = (0..super::blit::CANVASES)
            .filter_map(|slot| blit.image(slot))
            .collect();
        if self.pass.is_none() {
            self.pass = Some(Pass::new(&self.gpu, crate::dxgi::VK_FORMAT)?);
        }
        if let Some(pass) = self.pass.as_mut() {
            pass.set_targets(&images, width, height)?;
        }
        let t_targets = t_start.elapsed();
        // The canvases are new, so nothing has to be waited for on them yet.
        self.blit_value.set([0; super::blit::CANVASES]);
        self.blit = Some(blit);
        self.sdr_white.set(swapchain.sdr_white());
        self.hdr.set(swapchain.hdr());
        self.peak.set(swapchain.max_luminance());
        self.canvas.resized((width, height));
        eprintln!(
            "rebuild {width}x{height}: drain {:?} idle {:?} resize {:?} blit {:?} targets {:?} total {:?}",
            t_drain,
            t_idle - t_drain,
            t_resize - t_idle,
            t_blit - t_resize,
            t_targets - t_blit,
            t_start.elapsed()
        );
        Ok(())
    }

    /// Points the pass at the interface texture ZGUI composed into.
    pub fn set_interface(&mut self, texture: &wgpu::Texture) -> eros::Result<()> {
        if let Some(pass) = self.pass.as_mut() {
            pass.set_ui(texture)?;
        }
        Ok(())
    }

    /// Asks for another frame while the shell owns the window.
    ///
    /// Posted rather than invalidated here: winit calls `DefWindowProc` for the
    /// paint that produced this frame right after handing it over, and that
    /// validates whatever region we set. One message later the paint is over.
    pub fn request_frame(&self) {
        if !in_move_loop() {
            return;
        }
        // SAFETY: this is the window the chain presents into.
        let _ = unsafe { PostMessageW(Some(self.hwnd), NUDGE, WPARAM(0), LPARAM(0)) };
    }

    /// Waits for the last frame's pass to let go of the interface texture.
    ///
    /// The next compose overwrites the same texture, and ZGUI writes it with
    /// wgpu, which is a different set of submissions from this chain's. Nothing
    /// else orders the write after the read.
    pub fn wait_handover(&self) {
        if !self.sync.handover {
            return;
        }
        let (Some(swapchain), value) = (self.swapchain.as_ref(), self.submit.get()) else {
            return;
        };
        if value == 0 {
            return;
        }
        if !swapchain.wait_timeline(value, 1_000_000_000) {
            warn!(target: "ui", "the interface handover did not complete in a second");
        }
    }

    /// Whether the chain can present: a live swapchain the driver has not removed.
    pub fn presentable(&self) -> bool {
        self.swapchain
            .as_ref()
            .is_some_and(|chain| !chain.is_removed())
    }

    /// Whether there is room for another frame.
    ///
    /// There is nothing to wait for. The presentation is already paced twice
    /// over: `SetMaximumFrameLatency(2)` bounds the frames in flight, and
    /// `Present` blocks once the flip queue is full. The frame-latency waitable
    /// used to gate this, and it goes silent after `ResizeBuffers` — its event
    /// belongs to the presentation machinery the resize replaces — which turned
    /// into a permanent 10 fps (the 100 ms timeout) the first time the window
    /// was resized.
    #[expect(
        clippy::unused_self,
        reason = "the interface is shared with the Wayland chain, which has a queue to wait on"
    )]
    pub const fn wait_for_frame(&self, _timeout_ms: u32) -> bool {
        true
    }

    /// A no-op here, but the seam keeps the Wayland signature.
    #[expect(
        clippy::unused_self,
        reason = "the interface is shared with the Wayland chain, which needs the chain"
    )]
    pub const fn declare(&self, _width: u32, _height: u32, _scale: f32) {}

    pub fn configure(&mut self, ui: (u32, u32)) -> (u32, u32) {
        self.canvas.configured();
        if !self.canvas.covers(ui) {
            let (width, height) = resize_canvas(self.hwnd, ui.0, ui.1);
            if let Err(error) = self.rebuild(width, height) {
                warn!(target: "ui", "DXGI: could not grow the chain: {error:?}");
            }
        }
        // The UI texture always follows the window; only the chain's canvas
        // grows past it during a resize.
        ui
    }

    /// Gives the extra backbuffers back once the window's size has settled.
    pub fn settle(&mut self, ui: (u32, u32)) {
        if !self.canvas.settled() {
            return;
        }
        // A shrink must not land inside a drag: while the user is sizing the
        // window the desktop owns its frame, and `ResizeBuffers` there is what
        // was seen to take the device away. Wait for the drag to end.
        // SAFETY: `GetCapture` reads the calling thread's capture window.
        if !unsafe { windows::Win32::UI::Input::KeyboardAndMouse::GetCapture() }.is_invalid() {
            self.canvas.configured();
            return;
        }
        let target = idle_canvas(ui.0, ui.1);
        if self.canvas.size() == target {
            return;
        }
        if let Err(error) = self.rebuild(target.0, target.1) {
            warn!(target: "ui", "DXGI: could not shrink the chain: {error:?}");
        }
    }

    /// Re-reads the display under the running window, at most twice a second.
    fn refresh_display(&self, swapchain: &DxgiSwapchain) {
        // The shell owns the window during a move or a size drag, and DXGI work
        // in that loop is what was seen to take the device away. The state is
        // re-read the moment the drag ends.
        if in_move_loop() {
            return;
        }
        let now = Instant::now();
        let forced = DISPLAY_CHANGED.swap(false, std::sync::atomic::Ordering::Relaxed);
        if !forced
            && self
                .refreshed
                .get()
                .is_some_and(|last| now.duration_since(last) < REFRESH)
        {
            return;
        }
        self.refreshed.set(Some(now));
        if let Err(error) = swapchain.refresh() {
            warn!(target: "ui", "DXGI: display refresh failed: {error:?}");
            return;
        }
        let white = swapchain.sdr_white();
        if (white - self.sdr_white.get()).abs() > 0.5 {
            eprintln!("SDR white: {white:.0} nits (level from the display configuration)");
        }
        self.sdr_white.set(white);
        self.hdr.set(swapchain.hdr());
        self.peak.set(swapchain.max_luminance());
    }

    /// Acquires the backbuffer DXGI will present next, with the timeline values
    /// that hand it between Vulkan and D3D12.
    pub fn begin(&self, _ui: (u32, u32)) -> bool {
        let Some(swapchain) = self.swapchain.as_ref() else {
            return false;
        };
        if swapchain.is_removed() {
            return false;
        }
        self.refresh_display(swapchain);
        let Ok(frame) = swapchain.acquire() else {
            warn!(target: "ui", "DXGI: could not acquire a backbuffer");
            return false;
        };
        let Ok(index) = usize::try_from(frame.index) else {
            return false;
        };
        self.acquired.set(index);
        self.submit.set(frame.submit);
        true
    }

    /// Draws the interface over the fill into the acquired backbuffer and
    /// presents it.
    pub fn end(&self, ui: &wgpu::Texture) -> bool {
        let (Some(swapchain), Some(pass), Some(blit)) = (
            self.swapchain.as_ref(),
            self.pass.as_ref(),
            self.blit.as_ref(),
        ) else {
            return false;
        };
        let index = self.acquired.get();
        let Some(backbuffer) = swapchain.backbuffer(u32::try_from(index).unwrap_or(0)) else {
            return false;
        };
        let Some(command) = self.commands.get(index).copied() else {
            return false;
        };
        // The guard is scoped: wgpu's `poll` below takes the same snatch lock
        // and aborts on re-entry.
        let image = {
            // SAFETY: `as_hal` only asks that the texture outlives the borrow.
            let Some(hal) = (unsafe { ui.as_hal::<wgpu::hal::api::Vulkan>() }) else {
                warn!(target: "ui", "the interface texture is not Vulkan");
                return false;
            };
            // SAFETY: the texture is alive and owned by the caller.
            unsafe { hal.raw_handle() }
        };

        // The compose is wgpu's submission, not this chain's: nothing orders the
        // sample below after it. This is the edge `SIPHON_SYNC=compose` closes.
        if self.sync.compose
            && self
                .gpu
                .device()
                .poll(wgpu::PollType::wait_indefinitely())
                .is_err()
        {
            warn!(target: "ui", "the interface compose did not finish");
        }

        // SAFETY: the buffer was allocated from this pool and its frame has
        // retired, so it is not in use.
        if let Err(error) = unsafe {
            self.device
                .reset_command_buffer(command, vk::CommandBufferResetFlags::empty())
        } {
            warn!(target: "ui", "command buffer reset failed: {error:?}");
            return false;
        }
        // SAFETY: the command buffer is recording.
        if let Err(error) = unsafe {
            self.device.begin_command_buffer(
                command,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )
        } {
            warn!(target: "ui", "command buffer begin failed: {error:?}");
            return false;
        }
        let (width, height) = self.canvas.size();
        // Canvas `index` belongs to this backbuffer.
        let slot = index;
        pass.record(
            command,
            slot,
            image,
            (width, height),
            self.hdr.get(),
            self.sdr_white.get(),
        );
        // SAFETY: the command buffer is recording and closed once.
        if let Err(error) = unsafe { self.device.end_command_buffer(command) } {
            warn!(target: "ui", "command buffer end failed: {error:?}");
            return false;
        }

        let timeline = swapchain.timeline();
        // This canvas's own copy, three frames ago, must be done before the
        // render overwrites it. That value is long since signalled, so the wait
        // costs nothing; waiting on *last* frame's copy instead would cost a
        // tenth of a second, which is the other API's signal latency.
        let wait = self.blit_value.get().get(slot).copied().unwrap_or(0);
        let submit = self.submit.get();
        let waits = [vk::SemaphoreSubmitInfo::default()
            .semaphore(timeline)
            .value(wait)
            .stage_mask(vk::PipelineStageFlags2::COLOR_ATTACHMENT_OUTPUT)];
        let signals = [vk::SemaphoreSubmitInfo::default()
            .semaphore(timeline)
            .value(submit)
            .stage_mask(vk::PipelineStageFlags2::ALL_COMMANDS)];
        let infos = [vk::CommandBufferSubmitInfo::default().command_buffer(command)];
        let submit_info = [vk::SubmitInfo2::default()
            .wait_semaphore_infos(&waits)
            .signal_semaphore_infos(&signals)
            .command_buffer_infos(&infos)];
        // SAFETY: the queue and the buffer are live and the semaphore values
        // belong to this chain's timeline.
        if let Err(error) = unsafe { self.device.queue_submit2(self.queue, &submit_info, vk::Fence::null()) } {
            warn!(target: "ui", "queue submit failed: {error:?}");
            return false;
        }

        // D3D12 does the work that makes the present ordered: it waits for the
        // render, copies the canvas into the backbuffer, barriers that buffer
        // to PRESENT, and only then is the frame handed over.
        if let Err(error) = swapchain.wait_queue(submit) {
            warn!(target: "ui", "DXGI: queue wait failed: {error:?}");
            return false;
        }
        if let Err(error) = blit.copy(
            &self.d3d12,
            &backbuffer,
            slot,
            u32::try_from(index).unwrap_or(0),
        ) {
            warn!(target: "ui", "DXGI: canvas copy failed: {error:?}");
            return false;
        }
        let blit_value = swapchain.next_value();
        if let Err(error) = swapchain.signal_queue(blit_value) {
            warn!(target: "ui", "DXGI: queue signal failed: {error:?}");
            return false;
        }
        let mut values = self.blit_value.get();
        if let Some(slot_value) = values.get_mut(slot) {
            *slot_value = blit_value;
        }
        self.blit_value.set(values);

        if let Some((frame, count, first, last)) = pass.band() {
            eprintln!("band: frame {frame} count {count} rows {first}..{last}");
        }
        // D3D12 does the work that makes the present ordered: it waits for the
        // render, copies the canvas into the backbuffer, barriers that buffer
        // to PRESENT, and only then is the frame handed over.
        if let Err(error) = swapchain.wait_queue(submit) {
            warn!(target: "ui", "DXGI: queue wait failed: {error:?}");
            return false;
        }
        if let Err(error) = blit.copy(
            &self.d3d12,
            &backbuffer,
            slot,
            u32::try_from(index).unwrap_or(0),
        ) {
            warn!(target: "ui", "DXGI: canvas copy failed: {error:?}");
            return false;
        }
        let blit_value = swapchain.next_value();
        if let Err(error) = swapchain.signal_queue(blit_value) {
            warn!(target: "ui", "DXGI: queue signal failed: {error:?}");
            return false;
        }
        let mut values = self.blit_value.get();
        if let Some(slot_value) = values.get_mut(slot) {
            *slot_value = blit_value;
        }
        self.blit_value.set(values);

        if let Some((frame, count, first, last)) = pass.band() {
            eprintln!("band: frame {frame} count {count} rows {first}..{last}");
        }
        {
            thread_local! {
                static LAST: Cell<Option<Instant>> = const { Cell::new(None) };
            }
            LAST.with(|last| {
                if last.get().is_none_or(|at| at.elapsed().as_millis() >= 500) {
                    last.set(Some(Instant::now()));
                    // SAFETY: the fence is alive.
                    let done = unsafe { swapchain.fence().GetCompletedValue() };
                    eprintln!("fence: completed={done} submit={submit} blit={blit_value}");
                }
            });
        }

        // `SIPHON_SYNC=present` as an A/B against the copy's own ordering.
        if self.sync.present && !swapchain.wait_present(blit_value) {
            warn!(target: "ui", "the copy did not complete before present");
        }

        swapchain.present_only().is_ok()
    }

    /// The inner renderer's failures are already logged; this one needs a line.
    #[expect(
        clippy::unused_self,
        reason = "the interface is shared with the Wayland chain, which reports through the chain"
    )]
    pub fn present_failed(&self) {
        warn!(target: "ui", "DXGI present failed");
    }
}

impl Drop for Chain {
    fn drop(&mut self) {
        // SAFETY: the window outlives the chain, and removing a subclass that
        // is not installed is a no-op.
        let _ = unsafe { RemoveWindowSubclass(self.hwnd, Some(display_change_proc), SUBCLASS_ID) };
        // SAFETY: the pass and the swapchain are gone before the buffers that
        // name them; the device is idle so nothing is still reading the pool.
        unsafe {
            self.device.device_wait_idle().ok();
            self.device.destroy_command_pool(self.pool, None);
        }
        drop(self.pass.take());
        drop(self.swapchain.take());
    }
}
