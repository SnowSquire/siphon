//! ZGUI's renderer, presenting through the platform's chain.
//!
//! ZGUI composes the interface into an ordinary `Bgra8Unorm` texture and
//! libplacebo draws it into the frame the chain acquired. Which API presents
//! that frame — DXGI on Windows, a `VkSwapchainKHR` on Wayland — is the chain's
//! business, not this module's.

use std::any::Any;
use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

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
use zgui_render_wgpu::Gpu;
use zgui_render_wgpu::SharedGraphics;
use zgui_render_wgpu::WgpuRenderer;

use crate::chain::Chain;
use crate::placebo::Placebo;

pub struct UiRenderer {
    inner: WgpuRenderer,
    /// The platform's present chain: swapchain, its textures and its policy.
    chain: Option<Chain>,
    placebo: Rc<Placebo>,
    /// ZGUI's interface, wrapped for the overlay pass.
    ui: pl::pl_tex,
    /// Keeps the image behind `ui` alive; dropped after `ui` in `Drop`.
    ui_texture: wgpu::Texture,
    handover: pl::VkSemaphore,
    handover_value: Cell<u64>,
    graphics: SharedGraphics,
    gpu: Arc<Gpu>,
    /// The extent the UI texture and inner renderer are built for.
    width: u32,
    height: u32,
    /// A white texel mask for the diagnostics square, when it was asked for.
    nits_mask: Option<pl::pl_tex>,
}

impl UiRenderer {
    #[allow(clippy::too_many_arguments, reason = "one renderer wiring call")]
    pub fn new(
        inner: WgpuRenderer,
        chain: Chain,
        placebo: Rc<Placebo>,
        ui_texture: wgpu::Texture,
        graphics: SharedGraphics,
        gpu: Arc<Gpu>,
        target: RenderTarget,
    ) -> eros::Result<Self> {
        let ui = placebo.wrap_wgpu(&ui_texture)?;
        let (width, height) = (
            u32::try_from(target.size.width).unwrap_or(1),
            u32::try_from(target.size.height).unwrap_or(1),
        );

        let handover = placebo.create_semaphore();
        eros::ensure!(
            !handover.is_null(),
            "libplacebo refused to create the handover semaphore"
        );

        // A white texel for monochrome overlay parts: the diagnostics square,
        // and the fill that shows where the frame is not covered at all.
        let nits_mask = Some(placebo.create_flat(1, 1, [255, 255, 255, 255])?);

        Ok(Self {
            inner,
            chain: Some(chain),
            placebo,
            ui,
            ui_texture,
            handover,
            handover_value: Cell::new(0),
            graphics,
            gpu,
            width,
            height,
            nits_mask,
        })
    }

    /// The SDR colour space, for parts that are plain interface colours.
    fn srgb_space(chain: &Chain) -> pl::pl_color_space {
        let white = chain.sdr_white();
        pl::pl_color_space {
            primaries: pl::pl_color_primaries::PL_COLOR_PRIM_BT_709,
            transfer: pl::pl_color_transfer::PL_COLOR_TRC_SRGB,
            hdr: pl::pl_hdr_metadata {
                max_luma: white,
                max_cll: white,
                ..Default::default()
            },
        }
    }

    /// What the diagnostics square should be, in the presentation's own
    /// encoding: the signal for the output's peak on HDR, plain white on SDR.
    fn nits_square(chain: &Chain) -> (f32, pl::pl_color_space) {
        let white = chain.sdr_white();
        if chain.hdr() {
            let peak = chain.peak().max(white);
            return (
                crate::hdr::pq_signal(peak),
                pl::pl_color_space {
                    primaries: pl::pl_color_primaries::PL_COLOR_PRIM_BT_2020,
                    transfer: pl::pl_color_transfer::PL_COLOR_TRC_PQ,
                    hdr: pl::pl_hdr_metadata {
                        max_luma: peak,
                        max_cll: peak,
                        ..Default::default()
                    },
                },
            );
        }
        (
            1.0,
            pl::pl_color_space {
                primaries: pl::pl_color_primaries::PL_COLOR_PRIM_BT_709,
                transfer: pl::pl_color_transfer::PL_COLOR_TRC_SRGB,
                hdr: pl::pl_hdr_metadata {
                    max_luma: white,
                    max_cll: white,
                    ..Default::default()
                },
            },
        )
    }

    /// The interface overlay, in ZGUI's sRGB space.
    fn ui_overlay(&self, parts: &[pl::pl_overlay_part]) -> pl::pl_overlay {
        pl::pl_overlay {
            tex: self.ui,
            mode: pl::pl_overlay_mode::PL_OVERLAY_NORMAL,
            coords: pl::pl_overlay_coords::PL_OVERLAY_COORDS_DST_FRAME,
            repr: pl::pl_color_repr {
                sys: pl::pl_color_system::PL_COLOR_SYSTEM_RGB,
                levels: pl::pl_color_levels::PL_COLOR_LEVELS_FULL,
                // SIPHON_UI_OPAQUE: ignore the interface's alpha, to tell a
                // transparent region from one no part covers.
                alpha: if std::env::var_os("SIPHON_UI_OPAQUE").is_some() {
                    pl::pl_alpha_mode::PL_ALPHA_NONE
                } else {
                    pl::pl_alpha_mode::PL_ALPHA_PREMULTIPLIED
                },
                ..Default::default()
            },
            color: pl::pl_color_space {
                primaries: pl::pl_color_primaries::PL_COLOR_PRIM_BT_709,
                transfer: pl::pl_color_transfer::PL_COLOR_TRC_SRGB,
                hdr: pl::pl_hdr_metadata {
                    max_luma: self.chain.as_ref().map_or(200.0, Chain::sdr_white),
                    max_cll: self.chain.as_ref().map_or(200.0, Chain::sdr_white),
                    ..Default::default()
                },
            },
            parts: parts.as_ptr(),
            num_parts: parts.len() as std::os::raw::c_int,
        }
    }

    /// Paints the interface texture with the page background.
    ///
    /// ZGUI composes the regions it considers damaged, so a texture that was
    /// just created can be presented with rows nobody wrote yet. Those rows are
    /// transparent black, which shows as a hole in the overlay; starting from
    /// the page background makes a not-yet-painted row look like the interface
    /// instead of a band.
    fn clear_ui_texture(&self, texture: &wgpu::Texture) {
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self.gpu.device().create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        {
            let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("siphon.ui.background"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        // The page background, sRGB-encoded (#090E14).
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 9.0 / 255.0,
                            g: 14.0 / 255.0,
                            b: 18.0 / 255.0,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
        }
        self.gpu.queue().submit(Some(encoder.finish()));
    }

    /// TEMP: looks for the diagnostics fill in the frame and, if it is there,
    /// writes the frame and the geometry that produced it.
    ///
    /// Magenta is a colour the interface never contains, so a pixel of it can
    /// only be the underlay showing through. That means the frame is on disk
    /// the moment it happens, instead of being caught by inference.
    fn catch_band(&self, target: &pl::pl_frame) {
        let texture = target.planes[0].texture;
        if texture.is_null() {
            return;
        }
        // SAFETY: the frame names a live texture of this GPU.
        let (width, height) = unsafe { ((*texture).params.w, (*texture).params.h) };
        let (Ok(width), Ok(height)) = (usize::try_from(width), usize::try_from(height)) else {
            return;
        };
        let Some(count) = width.checked_mul(height) else {
            return;
        };
        let mut buffer = vec![0u32; count];
        let params = pl::pl_tex_transfer_params {
            tex: texture,
            rc: pl::pl_rect3d {
                x0: 0,
                y0: 0,
                z0: 0,
                x1: width as std::os::raw::c_int,
                y1: height as std::os::raw::c_int,
                z1: 1,
            },
            ptr: buffer.as_mut_ptr().cast(),
            ..Default::default()
        };
        // SAFETY: the buffer holds exactly width*height four-byte texels.
        if !unsafe { pl::pl_tex_download(self.placebo.gpu(), &params) } {
            return;
        }
        // 10-bit: magenta is (1023, 0, 1023).
        let mut band = None;
        for y in 0..height {
            let row = &buffer[y * width..(y + 1) * width];
            if row
                .iter()
                .any(|texel| texel & 0x3ff > 1000 && (texel >> 10) & 0x3ff < 20 && (texel >> 20) & 0x3ff > 1000)
            {
                band = Some(y);
                break;
            }
        }
        let Some(first) = band else {
            return;
        };
        eprintln!(
            "band: frame {width}x{height} ui {}x{} first row {first}",
            self.width, self.height
        );
        let path = std::env::temp_dir().join("siphon-band.bmp");
        if let Err(error) = write_bmp(&path, &buffer, width, height) {
            eprintln!("band: could not write {}: {error}", path.display());
        } else {
            eprintln!("band: wrote {}", path.display());
        }
    }

    /// Copies the interface's previous pixels into a freshly created texture.
    ///
    /// The overlap is 1:1 from the top-left, which is where the interface is
    /// anchored; pixels outside it stay at the background the clear left.
    fn carry_ui_texture(&self, texture: &wgpu::Texture) {
        let (old_width, old_height) = (self.width, self.height);
        let (new_width, new_height) = (texture.width(), texture.height());
        let width = old_width.min(new_width);
        let height = old_height.min(new_height);
        if width == 0 || height == 0 {
            return;
        }
        let mut encoder = self
            .gpu
            .device()
            .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        encoder.copy_texture_to_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.ui_texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
        self.gpu.queue().submit(Some(encoder.finish()));
    }

    /// Rebuilds the UI side of the presentation for a new target extent: the
    /// texture, its libplacebo view and the inner renderer. The chain resizes
    /// itself in [`Chain::configure`].
    fn rebuild(&mut self, target: RenderTarget, width: u32, height: u32) {
        // Nothing is replaced until its replacement exists: a step below that
        // fails returns with the interface this renderer already had, where
        // destroying it first would leave the frame that follows drawing
        // through a texture libplacebo has freed.
        let ui_texture = self.gpu.device().create_texture(&wgpu::TextureDescriptor {
            label: Some("siphon.ui"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Bgra8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_SRC
                | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        // A supplied texture is copied into by damage only and otherwise keeps
        // what it holds, so a fresh one has to start as the frame that was on
        // screen, not as a blank: the rows ZGUI is not owed this frame would
        // otherwise show as bands.
        self.clear_ui_texture(&ui_texture);
        self.carry_ui_texture(&ui_texture);
        let ui = match self.placebo.wrap_wgpu(&ui_texture) {
            Ok(ui) => ui,
            Err(error) => {
                tracing::warn!(target: "ui", "libplacebo: resized interface wrap failed: {error:?}");
                return;
            }
        };
        let mut inner = match self
            .graphics
            .renderer_supplied(target, vec![ui_texture.clone()])
        {
            Ok(inner) => inner,
            Err(error) => {
                tracing::warn!(target: "ui", "ZGUI: resize renderer failed: {error}");
                return;
            }
        };
        // The atlas *policy* lives above the renderer: the scene names its
        // glyph and image tiles by texture id and the window's content cache
        // believes those tiles are resident. A fresh renderer starts with
        // empty atlas textures, so without moving the old ones over every
        // piece of text would be sampled from a texture that no longer
        // exists. Rects are plain geometry and survive either way.
        std::mem::swap(self.inner.atlas(), inner.atlas());

        self.placebo.destroy_texture(&mut self.ui);
        self.ui = ui;
        self.ui_texture = ui_texture;
        self.inner = inner;
        self.width = width;
        self.height = height;
    }

    /// Applies a deferred configure: resize the chain, then rebuild the UI for
    /// the extent the chain actually granted.
    fn apply_configure(&mut self, target: RenderTarget) {
        let requested = (
            u32::try_from(target.size.width).unwrap_or(1),
            u32::try_from(target.size.height).unwrap_or(1),
        );
        let (width, height) = self
            .chain
            .as_mut()
            .map_or(requested, |chain| chain.configure(requested));
        self.rebuild(target, width, height);
    }

    fn present(&self, outcome: FrameOutcome) -> FrameOutcome {
        crate::chain::note_present();
        let Some(chain) = self.chain.as_ref() else {
            return outcome;
        };
        // SIPHON_PLAIN: ZGUI's texture straight into the swapchain, no
        // libplacebo anywhere in the path.
        if std::env::var_os("SIPHON_PLAIN").is_some() {
            if !chain.present_plain(&self.gpu, &self.ui_texture) {
                chain.present_failed();
            }
            return outcome;
        }
        if !chain.presentable() {
            return outcome;
        }
        // Re-arm before anything can bail out: a held drag has no damage of its
        // own, so the next frame must not depend on this one presenting.
        chain.request_frame();
        if !chain.wait_for_frame(100) {
            return outcome;
        }
        let Some(acquired) = chain.begin((self.width, self.height)) else {
            return outcome;
        };
        let (width, height) = (acquired.width, acquired.height);

        #[allow(
            clippy::cast_precision_loss,
            reason = "window sizes are well inside f32's exact integer range"
        )]
        let (ui_width_f, ui_height_f) = (self.width as f32, self.height as f32);
        let mut target = acquired.frame;

        // The canvas is larger than the UI whenever a drag is growing the
        // window. Extending the UI's own edge texels over the rest means the
        // area a frame exposes is the page background already, not black.
        let (width_f, height_f) = (width as f32, height as f32);
        let full = pl::pl_overlay_part {
            src: pl::pl_rect2df {
                x0: 0.0,
                y0: 0.0,
                x1: ui_width_f,
                y1: ui_height_f,
            },
            dst: pl::pl_rect2df {
                x0: 0.0,
                y0: 0.0,
                x1: ui_width_f,
                y1: ui_height_f,
            },
            color: [1.0, 1.0, 1.0, 1.0],
        };
        let mut parts = [full; 4];
        let mut count = 1;
        if width_f > ui_width_f {
            parts[count] = pl::pl_overlay_part {
                src: pl::pl_rect2df {
                    x0: ui_width_f - 1.0,
                    y0: 0.0,
                    x1: ui_width_f,
                    y1: ui_height_f,
                },
                dst: pl::pl_rect2df {
                    x0: ui_width_f,
                    y0: 0.0,
                    x1: width_f,
                    y1: ui_height_f,
                },
                color: [1.0, 1.0, 1.0, 1.0],
            };
            count += 1;
        }
        if height_f > ui_height_f {
            parts[count] = pl::pl_overlay_part {
                src: pl::pl_rect2df {
                    x0: 0.0,
                    y0: ui_height_f - 1.0,
                    x1: ui_width_f,
                    y1: ui_height_f,
                },
                dst: pl::pl_rect2df {
                    x0: 0.0,
                    y0: ui_height_f,
                    x1: ui_width_f,
                    y1: height_f,
                },
                color: [1.0, 1.0, 1.0, 1.0],
            };
            count += 1;
        }
        if width_f > ui_width_f && height_f > ui_height_f {
            parts[count] = pl::pl_overlay_part {
                src: pl::pl_rect2df {
                    x0: ui_width_f - 1.0,
                    y0: ui_height_f - 1.0,
                    x1: ui_width_f,
                    y1: ui_height_f,
                },
                dst: pl::pl_rect2df {
                    x0: ui_width_f,
                    y0: ui_height_f,
                    x1: width_f,
                    y1: height_f,
                },
                color: [1.0, 1.0, 1.0, 1.0],
            };
            count += 1;
        }
        // SIPHON_UI_CONST: draw the interface as one flat colour instead of the
        // texture. A band that survives this is not the texture's content.
        let mut flat = [pl::pl_overlay_part {
            src: pl::pl_rect2df {
                x0: 0.0,
                y0: 0.0,
                x1: 1.0,
                y1: 1.0,
            },
            dst: pl::pl_rect2df {
                x0: 0.0,
                y0: 0.0,
                x1: width_f,
                y1: height_f,
            },
            color: [0.5, 0.5, 0.5, 1.0],
        }];
        // The flat colour steps every frame, so a band of *stale* content shows
        // as a different shade while a band of *missing* content stays magenta.
        if std::env::var_os("SIPHON_UI_CONST").is_some() {
            static STEP: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let step = STEP.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % 16;
            #[allow(clippy::cast_precision_loss, reason = "sixteen steps")]
            let level = (step as f32).mul_add(0.6 / 16.0, 0.2);
            flat[0].color = [level, level, level, 1.0];
        }
        let overlay = if std::env::var_os("SIPHON_UI_CONST").is_some()
            && let Some(mask) = self.nits_mask
        {
            pl::pl_overlay {
                tex: mask,
                mode: pl::pl_overlay_mode::PL_OVERLAY_MONOCHROME,
                coords: pl::pl_overlay_coords::PL_OVERLAY_COORDS_DST_FRAME,
                repr: pl::pl_color_repr {
                    sys: pl::pl_color_system::PL_COLOR_SYSTEM_RGB,
                    levels: pl::pl_color_levels::PL_COLOR_LEVELS_FULL,
                    alpha: pl::pl_alpha_mode::PL_ALPHA_PREMULTIPLIED,
                    ..Default::default()
                },
                color: Self::srgb_space(chain),
                parts: flat.as_ptr(),
                num_parts: 1,
            }
        } else {
            self.ui_overlay(&parts[..count])
        };
        let mut overlays = [overlay; 4];
        let mut overlay_count = 0;
        let fill_part = [pl::pl_overlay_part {
            src: pl::pl_rect2df {
                x0: 0.0,
                y0: 0.0,
                x1: 1.0,
                y1: 1.0,
            },
            dst: pl::pl_rect2df {
                x0: 0.0,
                y0: 0.0,
                x1: width_f,
                y1: height_f,
            },
            color: [1.0, 0.0, 1.0, 1.0],
        }];
        // SIPHON_DEBUG_FILL: magenta under everything, so a region the overlay
        // parts do not cover is magenta rather than the pass's black clear.
        if std::env::var_os("SIPHON_DEBUG_FILL").is_some()
            && let Some(mask) = self.nits_mask
        {
            {
                overlays[0] = pl::pl_overlay {
                    tex: mask,
                    mode: pl::pl_overlay_mode::PL_OVERLAY_MONOCHROME,
                    coords: pl::pl_overlay_coords::PL_OVERLAY_COORDS_DST_FRAME,
                    repr: pl::pl_color_repr {
                        sys: pl::pl_color_system::PL_COLOR_SYSTEM_RGB,
                        levels: pl::pl_color_levels::PL_COLOR_LEVELS_FULL,
                        alpha: pl::pl_alpha_mode::PL_ALPHA_PREMULTIPLIED,
                        ..Default::default()
                    },
                    color: Self::srgb_space(chain),
                    parts: fill_part.as_ptr(),
                    num_parts: 1,
                };
                overlay_count = 1;
            }
        }
        // SIPHON_UI_NONE: the underlay alone, a frame of one colour.
        if std::env::var_os("SIPHON_UI_NONE").is_none() {
            overlays[overlay_count] = overlay;
            overlay_count += 1;
        }
        // SIPHON_UI_TWICE: the same overlay a second time. A band that survives
        // is not a single pass going missing.
        if std::env::var_os("SIPHON_UI_TWICE").is_some() && overlay_count < overlays.len() {
            overlays[overlay_count] = overlays[overlay_count - 1];
            overlay_count += 1;
        }
        let mut nits_part = [pl::pl_overlay_part {
            src: pl::pl_rect2df {
                x0: 0.0,
                y0: 0.0,
                x1: 1.0,
                y1: 1.0,
            },
            dst: pl::pl_rect2df {
                x0: 0.0,
                y0: 0.0,
                x1: 0.0,
                y1: 0.0,
            },
            color: [0.0; 4],
        }];
        // SIPHON_NITS_SQUARE: the 64 px diagnostic square in the top-right
        // corner, drawn at the output's peak so a capture can be measured. It
        // is a measurement overlay, not part of the interface, so it is only
        // drawn when asked for.
        if std::env::var_os("SIPHON_NITS_SQUARE").is_some()
            && let Some(mask) = self.nits_mask
        {
            {
                let (signal, space) = Self::nits_square(chain);
                let side = 64.0_f32;
                let inset = 24.0_f32;
                let right = ui_width_f - inset;
                let top = inset;
                nits_part[0].dst = pl::pl_rect2df {
                    x0: (right - side).max(0.0),
                    y0: top,
                    x1: right.max(side),
                    y1: top + side,
                };
                // Premultiplied alpha: the part's colour is its own mask value.
                nits_part[0].color = [signal, signal, signal, 1.0];
                overlays[overlay_count] = pl::pl_overlay {
                    tex: mask,
                    mode: pl::pl_overlay_mode::PL_OVERLAY_MONOCHROME,
                    coords: pl::pl_overlay_coords::PL_OVERLAY_COORDS_DST_FRAME,
                    repr: pl::pl_color_repr {
                        sys: pl::pl_color_system::PL_COLOR_SYSTEM_RGB,
                        levels: pl::pl_color_levels::PL_COLOR_LEVELS_FULL,
                        alpha: pl::pl_alpha_mode::PL_ALPHA_PREMULTIPLIED,
                        ..Default::default()
                    },
                    color: space,
                    parts: nits_part.as_ptr(),
                    num_parts: 1,
                };
                overlay_count += 1;
            }
        }
        target.overlays = overlays.as_ptr();
        target.num_overlays = i32::try_from(overlay_count).unwrap_or(0);

        // ZGUI just finished a pass into the interface texture, so it is in
        // `COLOR_ATTACHMENT_OPTIMAL`; hand it to libplacebo and take it back.
        self.placebo.release(
            self.ui,
            pl::VkImageLayout::VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL,
        );
        // SIPHON_BLIT: a plain copy of the interface texture instead of the
        // overlay render — is any of this coming from libplacebo's overlay path?
        if std::env::var_os("SIPHON_BLIT").is_some() {
            self.placebo
                .blit(self.ui, target.planes[0].texture);
            self.placebo.finish();
        } else if !self.placebo.render(None, &target) {
            tracing::warn!(target: "ui", "libplacebo: render failed for {width}x{height}");
            eprintln!("libplacebo: render failed for {width}x{height}");
        }

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
            tracing::warn!(target: "ui", "libplacebo: failed to take the interface texture back");
        }

        // Declare the frame's own extent, not the surface's possibly stale one.
        let scale = self
            .inner
            .target()
            .map_or(1.0, |target| f64::from(target.scale.get()));
        if std::env::var_os("SIPHON_CATCH_BANDS").is_some() {
            self.catch_band(&target);
        }
        if !chain.end(width, height, scale) {
            chain.present_failed();
        }
        outcome
    }
}

/// Writes a packed 10-bit frame as a 32-bit BMP, for the band trap.
fn write_bmp(
    path: &std::path::Path,
    frame: &[u32],
    width: usize,
    height: usize,
) -> std::io::Result<()> {
    let row = width * 4;
    let size = 54 + row * height;
    let mut out = Vec::with_capacity(size);
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&u32::try_from(size).unwrap_or(0).to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&54u32.to_le_bytes());
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&i32::try_from(width).unwrap_or(0).to_le_bytes());
    out.extend_from_slice(&i32::try_from(height).unwrap_or(0).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&32u16.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&u32::try_from(row * height).unwrap_or(0).to_le_bytes());
    out.extend_from_slice(&2835u32.to_le_bytes());
    out.extend_from_slice(&2835u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    for y in (0..height).rev() {
        for x in 0..width {
            let texel = frame[y * width + x];
            let red = (texel & 0x3ff) >> 2;
            let green = ((texel >> 10) & 0x3ff) >> 2;
            let blue = ((texel >> 20) & 0x3ff) >> 2;
            out.extend_from_slice(&[blue as u8, green as u8, red as u8, 255]);
        }
    }
    std::fs::write(path, out)
}

impl Drop for UiRenderer {
    fn drop(&mut self) {
        if let Some(mask) = self.nits_mask.take() {
            let mut mask = mask;
            self.placebo.destroy_texture(&mut mask);
        }
        self.placebo.destroy_texture(&mut self.ui);
        self.placebo.destroy_semaphore(&mut self.handover);
    }
}

impl Renderer for UiRenderer {
    fn capabilities(&self) -> RenderCapabilities {
        self.inner.capabilities()
    }

    fn configure(&mut self, target: RenderTarget) {
        let width = u32::try_from(target.size.width).unwrap_or(0);
        let height = u32::try_from(target.size.height).unwrap_or(0);
        if width == 0 || height == 0 {
            self.inner.configure(target);
            return;
        }
        if width == self.width && height == self.height {
            self.inner.configure(target);
            return;
        }
        // KWin computes the final frame position from the geometry declared at
        // the start of a maximize. Declare the new extent now, before the
        // swapchain is rebuilt, so the window lands at the origin instead of
        // centred on its old size.
        if let Some(chain) = self.chain.as_ref() {
            chain.declare(width, height, target.scale.get());
        }
        self.apply_configure(target);
    }

    fn target(&self) -> Option<RenderTarget> {
        self.inner.target()
    }

    fn draw(&mut self, scene: &Scene, damage: &DamageSet) -> FrameOutcome {
        if let Some(chain) = self.chain.as_mut() {
            chain.settle((self.width, self.height));
            // A display setting that needs a new chain is followed before this
            // frame acquires one: a chain's colour space is fixed at its creation.
            chain.apply_display();
        }
        if !self.inner.present_into(0) {
            tracing::warn!(target: "ui", "the UI texture was not supplied");
        }
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
