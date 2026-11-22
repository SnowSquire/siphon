//! The Windows chain: a D3D12 flip chain whose backbuffers are Vulkan images.
//!
//! The ZGUI interface is composed into a wgpu texture and libplacebo draws it
//! into the current backbuffer. The per-buffer `pl_tex` cache, the device-lost
//! latch and the monitor-sized resize canvas all live here; nothing above the
//! chain knows DXGI exists.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use eros::Context as _;
use libplacebo_sys as pl;
use raw_window_handle::HasWindowHandle as _;
use siphon_shared::settings::HdrMode;
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

use super::Acquired;
use super::Canvas;
use super::idle_canvas;
use crate::dxgi::BUFFERS;
use crate::dxgi::D3d12;
use crate::dxgi::DxgiSwapchain;
use crate::dxgi::hwnd_from_raw;
use crate::dxgi::resize_canvas;
use crate::dxgi::set_window_background;
use crate::placebo::Placebo;
use crate::settings::Display;

/// The presentation mode to ask DXGI for: `SIPHON_HDR=off|auto|on`, then the
/// client's own setting.
///
/// The environment variable is the escape hatch for a display left in HDR mode
/// that something cannot capture from, and for a test that must not depend on a
/// file; `off` forces SDR, `on` fails the chain when the output cannot present
/// PQ/Rec.2020. With neither set, `ui.json` decides, and its default follows the
/// output: PQ when the display is already in HDR, SDR otherwise.
fn hdr_mode(display: &Display) -> HdrMode {
    match std::env::var("SIPHON_HDR").as_deref() {
        Ok(value) if value.eq_ignore_ascii_case("off") => HdrMode::Off,
        Ok(value) if value.eq_ignore_ascii_case("on") => HdrMode::On,
        Ok(value) if value.eq_ignore_ascii_case("auto") => HdrMode::Auto,
        // No override in the environment: the client's own setting is what the
        // switch in Preferences writes.
        _ => display.present().hdr,
    }
}

/// The D3D12 device, the swapchain and the Vulkan views of its backbuffers.
pub struct Chain {
    d3d12: Rc<D3d12>,
    gpu: Arc<Gpu>,
    placebo: Rc<Placebo>,
    hwnd: HWND,
    swapchain: Option<DxgiSwapchain>,
    /// One libplacebo texture per backbuffer.
    targets: Vec<pl::pl_tex>,
    canvas: Canvas,
    /// The display state libplacebo grades against. Cells because a refresh
    /// runs under `&self` on the present path.
    sdr_white: Cell<f32>,
    hdr: Cell<bool>,
    peak: Cell<f32>,
    /// When the display was last re-read; the slider in Windows Settings moves
    /// without a resize, so the chain polls it a few times a second.
    refreshed: Cell<Option<Instant>>,
    /// Which backbuffers this chain owns right now, as opposed to libplacebo.
    ///
    /// A wrapped image starts held by its owner and must be released to
    /// libplacebo before it can be used; once libplacebo has drawn into it, it
    /// has to be held back — transitioned out of its render layout — before
    /// DXGI may present it.
    owned: Cell<[bool; BUFFERS as usize]>,
    /// The backbuffer the current frame acquired.
    acquired: Cell<usize>,
    /// Signalled when a backbuffer is handed back for presentation.
    target_sem: pl::VkSemaphore,
    target_value: Cell<u64>,
    /// The display settings this window is presented with.
    display: Rc<Display>,
}

/// How often the display state is re-read on the present path.
///
/// There is no event for the SDR content brightness slider — the only public
/// API is the getter — so this is a fallback poll. `WM_DISPLAYCHANGE` arrives
/// for mode changes (HDR on/off, resolution) and refreshes immediately.
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
        placebo: Rc<Placebo>,
        _surface: &Arc<dyn Surface>,
        gpu_surface: &Arc<dyn GpuSurface>,
        width: u32,
        height: u32,
        display: Rc<Display>,
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
        let target_sem = placebo.create_semaphore();
        eros::ensure!(
            !target_sem.is_null(),
            "libplacebo refused the backbuffer semaphore"
        );
        let mut this = Self {
            d3d12,
            gpu: Arc::clone(gpu),
            placebo,
            hwnd,
            swapchain: None,
            targets: Vec::new(),
            canvas: Canvas::new(width, height),
            sdr_white: Cell::new(200.0),
            hdr: Cell::new(false),
            peak: Cell::new(0.0),
            refreshed: Cell::new(None),
            owned: Cell::new([false; BUFFERS as usize]),
            acquired: Cell::new(0),
            target_sem,
            target_value: Cell::new(0),
            display,
        };
        this.rebuild(width, height)?;
        Ok(this)
    }

    /// The backbuffer's colour space.
    ///
    /// SDR is sRGB, not BT.1886: `DxgiSwapchain` hands the buffer to DXGI as
    /// `DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709`, the space every SDR desktop
    /// application writes and the one Windows treats as sRGB. Encoding the
    /// interface to γ2.4 and letting the compositor decode it as γ2.2 lifted
    /// the near-black background and washed the palette out. Declaring the
    /// space the overlay is already in makes the libplacebo pass an identity,
    /// which is also what the Wayland chain gets from its swapchain.
    ///
    /// HDR is PQ over Rec.2020 with the output's own peak, and the interface's
    /// SDR white (`sdr_white`, the user's SDR content brightness) as its
    /// reference white: libplacebo then places signal white at that many nits,
    /// which is the same place DWM would have put an SDR window instead of
    /// leaving the mapping to the compositor's HDR path.
    fn target_color(&self) -> pl::pl_color_space {
        if self.hdr.get() {
            return pl::pl_color_space {
                primaries: pl::pl_color_primaries::PL_COLOR_PRIM_BT_2020,
                transfer: pl::pl_color_transfer::PL_COLOR_TRC_PQ,
                hdr: pl::pl_hdr_metadata {
                    // Unknown peak: the SDR white is still the highest value
                    // this interface produces, so treating it as the peak is
                    // closer than letting the white point tone map.
                    max_luma: if self.peak.get() > 0.0 {
                        self.peak.get()
                    } else {
                        self.sdr_white.get()
                    },
                    max_cll: self.sdr_white.get(),
                    ..Default::default()
                },
            };
        }
        pl::pl_color_space {
            primaries: pl::pl_color_primaries::PL_COLOR_PRIM_BT_709,
            transfer: pl::pl_color_transfer::PL_COLOR_TRC_SRGB,
            hdr: pl::pl_hdr_metadata {
                max_luma: self.sdr_white.get(),
                max_cll: self.sdr_white.get(),
                ..Default::default()
            },
        }
    }

    /// Resizes the chain and wraps its new backbuffers, or creates it the first
    /// time. The caller has destroyed every libplacebo texture over the old
    /// images; the D3D12 resources they named go when `ResizeBuffers` runs.
    fn rebuild(&mut self, width: u32, height: u32) -> eros::Result<()> {
        let placebo = Rc::clone(&self.placebo);
        for mut target in self.targets.drain(..) {
            placebo.destroy_texture(&mut target);
        }
        // The views are gone, but the frames that used them may not be: the
        // images are freed next, and a driver handed a free while the GPU still
        // names the image is free to do anything.
        placebo.finish();

        // The chain is reused through `ResizeBuffers`; creating a new one would
        // make the window association and the flip queue start over. A chain
        // that cannot be resized is dropped, and the next attempt creates one.
        //
        // An HDR mode is not resizable: the colour space and the metadata are
        // chosen when the chain is created, so a new mode is a new chain.
        let hdr = hdr_mode(&self.display);
        if self
            .swapchain
            .as_ref()
            .is_some_and(|chain| chain.hdr_mode() != hdr)
        {
            self.swapchain = None;
        }
        let resized = match self.swapchain.as_mut() {
            Some(existing) => existing.resize(&self.d3d12, &self.gpu, width, height),
            None => DxgiSwapchain::new(
                &self.d3d12,
                &self.gpu,
                self.hwnd,
                width,
                height,
                // PQ when the client asks for it and the output is already in
                // HDR, SDR otherwise, so the interface is graded to the display
                // instead of being handed to DWM's SDR-in-HDR mapping.
                hdr,
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
        let mut targets = Vec::with_capacity(BUFFERS as usize);
        for index in 0..BUFFERS {
            let image = swapchain
                .image(index)
                .ok_or_else(|| eros::error!("the swapchain has no backbuffer {index}"))?;
            let target = placebo.wrap(
                image,
                width,
                height,
                ash::vk::Format::A2B10G10R10_UNORM_PACK32,
                ash::vk::ImageUsageFlags::COLOR_ATTACHMENT
                    | ash::vk::ImageUsageFlags::TRANSFER_SRC
                    | ash::vk::ImageUsageFlags::TRANSFER_DST
                    | ash::vk::ImageUsageFlags::SAMPLED,
            )?;
            placebo.release(target, pl::VkImageLayout::VK_IMAGE_LAYOUT_UNDEFINED);
            targets.push(target);
        }
        // Fresh wraps start held by their owner, exactly like creation.
        self.owned.set([false; BUFFERS as usize]);
        self.sdr_white.set(swapchain.sdr_white());
        self.hdr.set(swapchain.hdr());
        self.peak.set(swapchain.max_luminance());
        self.targets = targets;
        self.canvas.resized((width, height));
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

    /// Presents the interface texture with no libplacebo in the path.
    ///
    /// A test surface: ZGUI's wgpu (Vulkan) render is blitted straight into the
    /// DXGI backbuffer's VkImage and presented.
    pub fn present_plain(&self, gpu: &Gpu, ui: &wgpu::Texture) -> bool {
        let Some(swapchain) = self.swapchain.as_ref() else {
            return false;
        };
        if swapchain.is_removed() {
            return false;
        }
        let index = swapchain.current_index();
        let Some(image) = swapchain.image(index) else {
            return false;
        };
        if let Err(error) = super::blit_plain(gpu, ui, image, crate::dxgi::VK_FORMAT) {
            warn!(target: "ui", "plain present: {error:?}");
            return false;
        }
        swapchain.present(self.display.present().tearing).is_ok()
    }

    /// Whether the chain can present: a live swapchain the driver has not removed.
    pub fn presentable(&self) -> bool {
        self.swapchain
            .as_ref()
            .is_some_and(|chain| !chain.is_removed())
    }

    pub const fn sdr_white(&self) -> f32 {
        self.sdr_white.get()
    }

    /// Whether the chain is presenting PQ/Rec.2020.
    pub const fn hdr(&self) -> bool {
        self.hdr.get()
    }

    /// The output's peak luminance in nits, zero when it did not say.
    pub const fn peak(&self) -> f32 {
        self.peak.get()
    }

    /// Waits until the flip queue has room for another frame.
    pub fn wait_for_frame(&self, timeout_ms: u32) -> bool {
        self.swapchain
            .as_ref()
            .is_some_and(|chain| chain.wait_for_frame(timeout_ms))
    }

    /// Called when the window's extent changes. The UI follows it exactly; the
    /// chain grows to the monitor only while the window outgrows its canvas,
    /// and shrinks back in [`Chain::settle`].
    ///
    /// A no-op here, but the seam keeps the Wayland signature, where the
    /// compositor's configure wants the geometry declared before the swapchain
    /// is rebuilt.
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
    ///
    /// The SDR content brightness slider and the HDR switch move without a
    /// resize, so nothing else would tell the chain that the value it grades
    /// against has changed.
    fn refresh_display(&self, swapchain: &DxgiSwapchain) {
        // The shell owns the window during a move or a size drag, and DXGI
        // work in that loop is what was seen to take the device away. The
        // state is re-read the moment the drag ends.
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

    /// Acquires the backbuffer DXGI will present next and builds its frame.
    pub fn begin(&self, _ui: (u32, u32)) -> Option<Acquired> {
        let swapchain = self.swapchain.as_ref()?;
        if swapchain.is_removed() {
            return None;
        }
        self.refresh_display(swapchain);
        let (width, height) = self.canvas.size();
        let index = usize::try_from(swapchain.current_index()).ok()?;
        let texture = *self.targets.get(index)?;
        // Hand this buffer to libplacebo for the frame, if we took it back
        // after presenting it last time.
        if self.owned.get()[index] {
            self.placebo
                .release(texture, pl::VkImageLayout::VK_IMAGE_LAYOUT_GENERAL);
            let mut owned = self.owned.get();
            owned[index] = false;
            self.owned.set(owned);
        }
        self.acquired.set(index);
        let mut frame = pl::pl_frame {
            num_planes: 1,
            planes: [pl::pl_plane {
                texture,
                components: 3,
                component_mapping: [0, 1, 2, 0],
                ..Default::default()
            }; 4],
            repr: pl::pl_color_repr {
                sys: pl::pl_color_system::PL_COLOR_SYSTEM_RGB,
                levels: pl::pl_color_levels::PL_COLOR_LEVELS_FULL,
                alpha: pl::pl_alpha_mode::PL_ALPHA_NONE,
                ..Default::default()
            },
            color: self.target_color(),
            ..Default::default()
        };
        frame.crop = pl::pl_rect2df {
            x0: 0.0,
            y0: 0.0,
            x1: width as f32,
            y1: height as f32,
        };
        Some(Acquired {
            frame,
            width,
            height,
        })
    }

    /// Follows a display setting that needs a new chain.
    ///
    /// `hdr` picks the colour space and the metadata at creation, so it cannot
    /// change in place; the window's extent stays what it was.
    pub fn apply_display(&mut self) {
        let wanted = hdr_mode(&self.display);
        let changed = self
            .swapchain
            .as_ref()
            .is_some_and(|chain| chain.hdr_mode() != wanted);
        if !changed {
            return;
        }
        let (width, height) = self.canvas.size();
        if let Err(error) = self.rebuild(width, height) {
            warn!(target: "ui", "DXGI: could not change the display mode: {error:?}");
        }
    }

    /// Hands the backbuffer to the compositor.
    pub fn end(&self, _width: u32, _height: u32, _scale: f64) -> bool {
        let Some(swapchain) = self.swapchain.as_ref() else {
            return false;
        };
        // Take the backbuffer back in a layout DXGI can present. Without this
        // the image is still owned by Vulkan, in a render layout, when Present
        // reads it.
        let index = self.acquired.get();
        if let Some(texture) = self.targets.get(index) {
            let value = self.target_value.get() + 1;
            let sem = pl::pl_vulkan_sem {
                sem: self.target_sem,
                value,
            };
            if self
                .placebo
                .hold(*texture, pl::VkImageLayout::VK_IMAGE_LAYOUT_GENERAL, sem)
            {
                self.target_value.set(value);
                let mut owned = self.owned.get();
                owned[index] = true;
                self.owned.set(owned);
            } else {
                warn!(target: "ui", "DXGI: could not take backbuffer {index} back");
                return false;
            }
        }
        // One wait for the frame: the render, the interface handover and the
        // backbuffer transition are all complete by the time Present runs.
        self.placebo.finish();
        swapchain.present(self.display.present().tearing).is_ok()
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
        if !self.target_sem.is_null() {
            let mut sem = self.target_sem;
            self.placebo.destroy_semaphore(&mut sem);
        }
        // SAFETY: the window outlives the chain, and removing a subclass that
        // is not installed is a no-op.
        let _ = unsafe { RemoveWindowSubclass(self.hwnd, Some(display_change_proc), SUBCLASS_ID) };
        // The views name the chain's images, so they go before it. The
        // swapchain's own Drop releases the D3D12 resources.
        for mut target in self.targets.drain(..) {
            self.placebo.destroy_texture(&mut target);
        }
    }
}
