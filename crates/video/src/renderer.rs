//! The renderer ZGUI draws through: the stock wgpu renderer, then one libplacebo
//! pass that puts the video and the interface into a DXGI backbuffer.
//!
//! ZGUI composes its interface into an ordinary `Bgra8Unorm` texture. libplacebo
//! renders the decoded frame into the current backbuffer, tone mapping it for the
//! display, and blends the interface over it as an overlay in the same pass. The
//! backbuffer is then handed to DXGI, which is the only presentation path: there
//! is no `VkSurfaceKHR` and no `VkSwapchainKHR` anywhere.

use std::any::Any;
use std::cell::Cell;
use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;
use std::time::Instant;

use ffmpeg_the_third::frame;
use libplacebo_sys as pl;
use zgui::atlas::TextureSink;
use zgui::bits::DamageSet;
use zgui::render::ExternalTexture;
use zgui::render::FrameOutcome;
use zgui::render::MemoryReport;
use zgui::render::RenderCapabilities;
use zgui::render::RenderTarget;
use zgui::render::Renderer;
use zgui::render::ScrollShift;
use zgui::render::TargetPoolReport;
use zgui::render::TextureHandle;
use zgui::render::VectorStatus;
use zgui::scene::Scene;
use zgui::surface::wgpu;
use zgui_render_wgpu::WgpuRenderer;

use crate::dxgi::DxgiSwapchain;
use crate::placebo::Placebo;
use crate::player::MappedFrame;
use crate::player::Player;

pub struct DxgiRenderer {
    inner: WgpuRenderer,
    swapchain: Rc<RefCell<DxgiSwapchain>>,
    placebo: Placebo,
    /// One libplacebo texture per backbuffer.
    targets: Vec<pl::pl_tex>,
    /// ZGUI's interface, wrapped for the overlay pass.
    ui: pl::pl_tex,
    /// Keeps the image behind `ui` alive; dropped after `ui` in `Drop`.
    _ui_texture: wgpu::Texture,
    /// Absent when no file was given, which leaves the interface on its own.
    player: Option<Player>,
    /// The frame being shown, with the instant it became due.
    current: Option<(MappedFrame, Instant)>,
    /// The stream time of the first frame, and when the clock started.
    clock: Option<(f64, Instant)>,
    /// A frame decoded ahead of time, kept until it is due.
    pending: Option<(frame::Video, f64)>,
    /// The display's peak luminance, for the target colour space.
    peak_nits: Cell<f32>,
    /// The display's SDR white point, for the interface and SDR video.
    sdr_white: f32,
    /// Timeline semaphore for handing the interface texture back to wgpu.
    handover: pl::VkSemaphore,
    /// Monotonic value the last handover signalled.
    handover_value: Cell<u64>,
    /// Whether the end of the stream was already reported.
    eof: Cell<bool>,
    /// Whether the colour spaces were already reported.
    logged: Cell<bool>,
    /// Whether the frame just composed carries interface damage.
    damaged: Cell<bool>,
    /// Whether anything has been presented yet.
    presented: Cell<bool>,
    /// Frames shown and frames presented, for the end-of-stream line.
    shown: Cell<u64>,
    presents: Cell<u64>,
}

impl DxgiRenderer {
    pub fn new(
        inner: WgpuRenderer,
        swapchain: DxgiSwapchain,
        placebo: Placebo,
        ui_texture: wgpu::Texture,
        player: Option<Player>,
    ) -> anyhow::Result<Self> {
        let ui = placebo.wrap_wgpu(&ui_texture)?;
        let peak_nits = Cell::new(swapchain.max_luminance());
        let sdr_white = swapchain.sdr_white();

        let (width, height) = swapchain.size();
        let mut targets = Vec::with_capacity(crate::dxgi::BUFFERS as usize);
        for index in 0..crate::dxgi::BUFFERS {
            let image = swapchain
                .image(index)
                .ok_or_else(|| anyhow::anyhow!("the swapchain has no backbuffer {index}"))?;
            let target = placebo.wrap(
                image,
                width,
                height,
                // Must match the usage `dxgi` created the images with.
                ash::vk::Format::A2B10G10R10_UNORM_PACK32,
                ash::vk::ImageUsageFlags::COLOR_ATTACHMENT
                    | ash::vk::ImageUsageFlags::TRANSFER_SRC
                    | ash::vk::ImageUsageFlags::TRANSFER_DST
                    | ash::vk::ImageUsageFlags::SAMPLED,
            )?;
            // Only libplacebo renders into these, so it keeps them from here on.
            placebo.release(target, pl::VkImageLayout::VK_IMAGE_LAYOUT_UNDEFINED);
            targets.push(target);
        }

        let handover = placebo.create_semaphore();
        anyhow::ensure!(
            !handover.is_null(),
            "libplacebo refused to create the handover semaphore"
        );

        Ok(Self {
            inner,
            swapchain: Rc::new(RefCell::new(swapchain)),
            placebo,
            targets,
            ui,
            _ui_texture: ui_texture,
            player,
            current: None,
            clock: None,
            pending: None,
            peak_nits,
            sdr_white,
            handover,
            handover_value: Cell::new(0),
            eof: Cell::new(false),
            logged: Cell::new(false),
            damaged: Cell::new(true),
            presented: Cell::new(false),
            shown: Cell::new(0),
            presents: Cell::new(0),
        })
    }

    /// The colour space the swapchain expects its pixels in.
    fn target_color(&self) -> pl::pl_color_space {
        if !self.swapchain.borrow().hdr() {
            return pl::pl_color_space {
                primaries: pl::pl_color_primaries::PL_COLOR_PRIM_BT_709,
                transfer: pl::pl_color_transfer::PL_COLOR_TRC_BT_1886,
                hdr: pl::pl_hdr_metadata {
                    max_luma: self.sdr_white,
                    max_cll: self.sdr_white,
                    ..Default::default()
                },
            };
        }
        let peak = self.peak_nits.get();
        pl::pl_color_space {
            primaries: pl::pl_color_primaries::PL_COLOR_PRIM_BT_2020,
            transfer: pl::pl_color_transfer::PL_COLOR_TRC_PQ,
            hdr: pl::pl_hdr_metadata {
                // A display with no reported peak: PQ's own 10 000 nits is wrong
                // for tone mapping, so fall back to a typical HDR panel.
                max_luma: if peak > 0.0 { peak } else { 1000.0 },
                min_luma: 0.0,
                max_cll: if peak > 0.0 { peak } else { 1000.0 },
                max_fall: 0.0,
                ..Default::default()
            },
        }
    }

    /// The interface overlay, in the interface texture's own colour space.
    fn ui_overlay(&self, part: &pl::pl_overlay_part) -> pl::pl_overlay {
        pl::pl_overlay {
            tex: self.ui,
            mode: pl::pl_overlay_mode::PL_OVERLAY_NORMAL,
            coords: pl::pl_overlay_coords::PL_OVERLAY_COORDS_DST_FRAME,
            repr: pl::pl_color_repr {
                sys: pl::pl_color_system::PL_COLOR_SYSTEM_RGB,
                levels: pl::pl_color_levels::PL_COLOR_LEVELS_FULL,
                // ZGUI composites its scene premultiplied.
                alpha: pl::pl_alpha_mode::PL_ALPHA_PREMULTIPLIED,
                ..Default::default()
            },
            color: pl::pl_color_space {
                primaries: pl::pl_color_primaries::PL_COLOR_PRIM_BT_709,
                transfer: pl::pl_color_transfer::PL_COLOR_TRC_SRGB,
                hdr: pl::pl_hdr_metadata {
                    // An SDR interface is relative to the display's white, not
                    // to libplacebo's 203-nit default.
                    max_luma: self.sdr_white,
                    max_cll: self.sdr_white,
                    ..Default::default()
                },
            },
            parts: part,
            num_parts: 1,
        }
    }

    /// Shows the frame that is due, decoding ahead as far as the clock allows.
    ///
    /// The runtime draws more often than the video changes frames, so decoding
    /// one frame per draw would play the file several times too fast. The stream
    /// timestamps are the only clock there is: a frame is shown from its
    /// timestamp until the next one's.
    fn advance(&mut self) -> bool {
        let Some(player) = self.player.as_mut() else {
            return false;
        };
        let now = Instant::now();
        let mut changed = false;
        loop {
            if let Some((_, due)) = &self.current
                && *due > now
            {
                return changed;
            }
            let (picture, pts) = if let Some(pending) = self.pending.take() {
                pending
            } else {
                match player.decode() {
                    Ok(Some(picture)) => {
                        let pts = player.timestamp(&picture);
                        (picture, pts)
                    }
                    Ok(None) => {
                        let elapsed = self
                            .clock
                            .map_or(0.0, |(_, started)| started.elapsed().as_secs_f64());
                        if !self.eof.replace(true) {
                            eprintln!(
                                "video: end of stream after {elapsed:.2} s                              ({} frames shown, {} presented)",
                                self.shown.get(),
                                self.presents.get()
                            );
                        }
                        return changed;
                    }
                    Err(error) => {
                        eprintln!("video: {error:#}");
                        return changed;
                    }
                }
            };
            let (first_pts, started) = *self.clock.get_or_insert((pts, now));
            #[allow(
                clippy::cast_precision_loss,
                reason = "a stream is minutes long, where f64 is exact to the nanosecond"
            )]
            let due = started + Duration::from_secs_f64((pts - first_pts).max(0.0));
            if due > now {
                self.pending = Some((picture, pts));
                return changed;
            }
            match MappedFrame::map(self.placebo.gpu(), &picture) {
                Ok(mut mapped) => {
                    mapped.set_sdr_white(self.sdr_white);
                    self.current = Some((mapped, due));
                    self.shown.set(self.shown.get() + 1);
                    changed = true;
                }
                Err(error) => eprintln!("libplacebo: {error:#}"),
            }
        }
    }

    /// Composes one frame: decode, render video plus interface, present.
    ///
    /// Called for every frame the runtime runs, including the ones it considers
    /// undamaged, because the video changes even when the interface does not.
    #[allow(
        clippy::cast_precision_loss,
        reason = "frame sizes and nits are well inside f32's exact integer range"
    )]
    fn present(&mut self, outcome: FrameOutcome) -> FrameOutcome {
        let (width, height) = self.swapchain.borrow().size();
        let index = self.swapchain.borrow().current_index();
        let Some(&target_tex) = self.targets.get(usize::try_from(index).unwrap_or(0)) else {
            return outcome;
        };

        let advanced = self.advance();
        let damaged = self.damaged.replace(false);
        // Variable refresh follows the present rate, so a duplicate present
        // would run the panel at the interface's repaint rate instead of the
        // video's. Present only when something actually changed.
        if !advanced && !damaged && self.presented.get() {
            return outcome;
        }
        if !self.swapchain.borrow().wait_for_frame(100) {
            // The compositor is not taking frames (occluded or minimized).
            // Dropping this one beats blocking the event loop; the next pump
            // tick asks again.
            self.damaged.set(damaged);
            return outcome;
        }
        let image = self.current.as_ref().map(|(frame, _)| frame.frame());
        if !self.logged.replace(true) {
            if let Some(image) = image {
                eprintln!(
                    "source: {:?}/{:?}, repr {:?}, mastering {:.0} nits",
                    image.color.primaries,
                    image.color.transfer,
                    image.repr.sys,
                    image.color.hdr.max_luma
                );
            }
            let target = self.target_color();
            eprintln!(
                "target: {:?}/{:?}, peak {:.0} nits",
                target.primaries, target.transfer, target.hdr.max_luma
            );
        }

        let mut target = pl::pl_frame {
            num_planes: 1,
            planes: [pl::pl_plane {
                texture: target_tex,
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
        target.crop = image.map_or(
            pl::pl_rect2df {
                x0: 0.0,
                y0: 0.0,
                x1: width as f32,
                y1: height as f32,
            },
            |frame| fit(aspect_of(frame), width, height),
        );

        // The interface covers the whole frame, in destination pixels.
        let part = pl::pl_overlay_part {
            src: pl::pl_rect2df {
                x0: 0.0,
                y0: 0.0,
                x1: width as f32,
                y1: height as f32,
            },
            dst: pl::pl_rect2df {
                x0: 0.0,
                y0: 0.0,
                x1: width as f32,
                y1: height as f32,
            },
            color: [1.0, 1.0, 1.0, 1.0],
        };
        let overlay = self.ui_overlay(&part);
        target.overlays = &raw const overlay;
        target.num_overlays = 1;

        // ZGUI just finished a render pass into the interface texture, so it is
        // in `COLOR_ATTACHMENT_OPTIMAL`; libplacebo samples from it, and has to
        // put it back there for the next frame's pass.
        self.placebo.release(
            self.ui,
            pl::VkImageLayout::VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL,
        );

        if !self.placebo.render(image, &target) {
            eprintln!("libplacebo: render failed for {width}x{height}");
        }
        // Coarse: the flip must not see an unfinished render pass. A fence or a
        // DXGI frame-latency waitable replaces this once the pipeline is right.
        self.placebo.finish();

        let value = self.handover_value.get() + 1;
        self.handover_value.set(value);
        let held = self.placebo.hold(
            self.ui,
            pl::VkImageLayout::VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL,
            pl::pl_vulkan_sem {
                sem: self.handover,
                value,
            },
        );
        if !held {
            eprintln!("libplacebo: failed to take the interface texture back");
        }

        if let Err(error) = self.swapchain.borrow().present() {
            eprintln!("DXGI present failed: {error:#}");
        }
        self.presented.set(true);
        self.presents.set(self.presents.get() + 1);
        outcome
    }
}

impl Drop for DxgiRenderer {
    fn drop(&mut self) {
        // The wrapped textures name images owned by the swapchain and the wgpu
        // texture below, so they are released first. Everything is idle: the
        // last `present` finished the GPU before presenting.
        for target in &mut self.targets {
            self.placebo.destroy_texture(target);
        }
        self.placebo.destroy_texture(&mut self.ui);
        self.placebo.destroy_semaphore(&mut self.handover);
    }
}

/// The destination rectangle of a frame with `aspect`, centred and letterboxed.
fn fit(aspect: f32, width: u32, height: u32) -> pl::pl_rect2df {
    #[allow(
        clippy::cast_precision_loss,
        reason = "frame sizes are well inside f32's exact integer range"
    )]
    let (width, height) = (width as f32, height as f32);
    if aspect > width / height {
        let cropped = width / aspect;
        let y0 = (height - cropped) / 2.0;
        pl::pl_rect2df {
            x0: 0.0,
            y0,
            x1: width,
            y1: y0 + cropped,
        }
    } else {
        let cropped = height * aspect;
        let x0 = (width - cropped) / 2.0;
        pl::pl_rect2df {
            x0,
            y0: 0.0,
            x1: x0 + cropped,
            y1: height,
        }
    }
}

/// The display aspect ratio of a mapped frame, crop included.
fn aspect_of(frame: &pl::pl_frame) -> f32 {
    let width = frame.crop.x1 - frame.crop.x0;
    let height = frame.crop.y1 - frame.crop.y0;
    if width <= 0.0 || height <= 0.0 {
        return 16.0 / 9.0;
    }
    width / height * frame.pixel_aspect_ratio.max(f32::MIN_POSITIVE)
}

impl Renderer for DxgiRenderer {
    fn capabilities(&self) -> RenderCapabilities {
        self.inner.capabilities()
    }

    fn configure(&mut self, target: RenderTarget) {
        self.inner.configure(target);
    }

    fn target(&self) -> Option<RenderTarget> {
        self.inner.target()
    }

    fn draw(&mut self, scene: &Scene, damage: &DamageSet) -> FrameOutcome {
        // The inner renderer has one destination: the interface texture. The
        // DXGI backbuffer is written by the libplacebo pass in `present_held`,
        // which the platform calls once per frame it is ready for.
        if !self.inner.present_into(0) {
            eprintln!("the UI texture was not supplied");
        }
        self.damaged.set(self.damaged.get() || !damage.is_empty());
        let outcome = self.inner.draw(scene, damage);
        self.present(outcome)
    }

    fn shifts_composed_pixels(&self) -> bool {
        self.inner.shifts_composed_pixels()
    }

    fn composed_survives_configure(&self) -> bool {
        self.inner.composed_survives_configure()
    }

    fn shift_composed(&mut self, shift: ScrollShift) {
        self.inner.shift_composed(shift);
    }

    fn register_external(&mut self, texture: ExternalTexture) -> TextureHandle {
        self.inner.register_external(texture)
    }

    fn release_external(&mut self, handle: TextureHandle) {
        self.inner.release_external(handle);
    }

    fn memory(&self) -> MemoryReport {
        self.inner.memory()
    }

    fn vector_status(&self) -> VectorStatus {
        self.inner.vector_status()
    }

    fn target_pool(&self) -> TargetPoolReport {
        self.inner.target_pool()
    }

    fn release_cached_targets(&mut self) -> u64 {
        self.inner.release_cached_targets()
    }

    fn release_idle_resources(&mut self) -> u64 {
        self.inner.release_idle_resources()
    }

    fn acquire_block(&self) -> Duration {
        self.inner.acquire_block()
    }

    fn present_held(&mut self) -> FrameOutcome {
        // The runtime never asks for this window's frame to be handed over
        // separately; `draw` presents into DXGI itself.
        self.inner.present_held()
    }

    fn texture_sink(&mut self) -> &mut dyn TextureSink {
        self.inner.texture_sink()
    }

    fn as_any_mut(&mut self) -> Option<&mut dyn Any> {
        self.inner.as_any_mut()
    }

    fn as_any(&self) -> Option<&dyn Any> {
        self.inner.as_any()
    }
}
