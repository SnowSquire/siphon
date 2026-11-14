//! ZGUI's renderer, presenting through the DXGI backbuffer.
//!
//! ZGUI composes the interface into an ordinary `Bgra8Unorm` texture, libplacebo
//! puts it into the current DXGI backbuffer, and DXGI presents. There is no
//! `VkSurfaceKHR` and no `VkSwapchainKHR`; the backbuffers are D3D12 resources
//! imported into Vulkan.

use std::any::Any;
use std::cell::Cell;
use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use libplacebo_sys as pl;
use siphon_rpc::Snapshot;
use windows::Win32::Foundation::HWND;
use windows_sys::Win32::UI::WindowsAndMessaging::PostMessageW;
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOW;
use windows_sys::Win32::UI::WindowsAndMessaging::SetForegroundWindow;
use windows_sys::Win32::UI::WindowsAndMessaging::ShowWindow;
use windows_sys::Win32::UI::WindowsAndMessaging::WM_CLOSE;
use zgui::atlas::TextureSink;
use zgui::bits::DamageSet;
use zgui::prelude::*;
use zgui::reactive::LocalStorage;
use zgui::reactive::RwSignal;
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

use crate::dxgi::BUFFERS;
use crate::dxgi::DxgiSwapchain;
use crate::placebo::Placebo;
use crate::rpc::UiEvent;
use crate::rpc::apply_update;
use crate::tray::TrayAction;

pub struct UiRenderer {
    inner: WgpuRenderer,
    swapchain: Rc<RefCell<DxgiSwapchain>>,
    placebo: Placebo,
    /// One libplacebo texture per backbuffer.
    targets: Vec<pl::pl_tex>,
    /// ZGUI's interface, wrapped for the overlay pass.
    ui: pl::pl_tex,
    /// Keeps the image behind `ui` alive; dropped after `ui` in `Drop`.
    _ui_texture: wgpu::Texture,
    sdr_white: f32,
    handover: pl::VkSemaphore,
    handover_value: Cell<u64>,
    damaged: Cell<bool>,
    presented: Cell<bool>,
    hwnd: HWND,
    snapshot: RwSignal<Snapshot, LocalStorage>,
    events: kanal::Receiver<UiEvent>,
    tray: kanal::Receiver<TrayAction>,
}

impl UiRenderer {
    #[allow(clippy::too_many_arguments, reason = "one renderer wiring call")]
    pub fn new(
        inner: WgpuRenderer,
        swapchain: DxgiSwapchain,
        placebo: Placebo,
        ui_texture: wgpu::Texture,
        snapshot: RwSignal<Snapshot, LocalStorage>,
        events: kanal::Receiver<UiEvent>,
        tray: kanal::Receiver<TrayAction>,
        hwnd: HWND,
    ) -> anyhow::Result<Self> {
        let ui = placebo.wrap_wgpu(&ui_texture)?;
        let sdr_white = swapchain.sdr_white();
        let (width, height) = swapchain.size();
        let mut targets = Vec::with_capacity(BUFFERS as usize);
        for index in 0..BUFFERS {
            let image = swapchain
                .image(index)
                .ok_or_else(|| anyhow::anyhow!("the swapchain has no backbuffer {index}"))?;
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
            sdr_white,
            handover,
            handover_value: Cell::new(0),
            damaged: Cell::new(true),
            presented: Cell::new(false),
            hwnd,
            snapshot,
            events,
            tray,
        })
    }

    /// The backbuffer's SDR colour space.
    fn target_color(&self) -> pl::pl_color_space {
        pl::pl_color_space {
            primaries: pl::pl_color_primaries::PL_COLOR_PRIM_BT_709,
            transfer: pl::pl_color_transfer::PL_COLOR_TRC_BT_1886,
            hdr: pl::pl_hdr_metadata {
                max_luma: self.sdr_white,
                max_cll: self.sdr_white,
                ..Default::default()
            },
        }
    }

    /// The interface overlay, in ZGUI's sRGB space.
    fn ui_overlay(&self, part: &pl::pl_overlay_part) -> pl::pl_overlay {
        pl::pl_overlay {
            tex: self.ui,
            mode: pl::pl_overlay_mode::PL_OVERLAY_NORMAL,
            coords: pl::pl_overlay_coords::PL_OVERLAY_COORDS_DST_FRAME,
            repr: pl::pl_color_repr {
                sys: pl::pl_color_system::PL_COLOR_SYSTEM_RGB,
                levels: pl::pl_color_levels::PL_COLOR_LEVELS_FULL,
                alpha: pl::pl_alpha_mode::PL_ALPHA_PREMULTIPLIED,
                ..Default::default()
            },
            color: pl::pl_color_space {
                primaries: pl::pl_color_primaries::PL_COLOR_PRIM_BT_709,
                transfer: pl::pl_color_transfer::PL_COLOR_TRC_SRGB,
                hdr: pl::pl_hdr_metadata {
                    max_luma: self.sdr_white,
                    max_cll: self.sdr_white,
                    ..Default::default()
                },
            },
            parts: part,
            num_parts: 1,
        }
    }

    /// Drains RPC events into the reactive snapshot and handles tray actions.
    /// Runs on the UI thread, before the scene is composed for this frame.
    fn sync(&self) {
        let mut state = self.snapshot.get_untracked();
        let mut changed = false;
        while let Ok(Some(event)) = self.events.try_recv() {
            match event {
                UiEvent::Snapshot(snapshot) => state = *snapshot,
                UiEvent::Update(update) => apply_update(&mut state, update),
            }
            changed = true;
        }
        if changed {
            self.snapshot.set(state);
            self.damaged.set(true);
        }

        while let Ok(Some(action)) = self.tray.try_recv() {
            match action {
                TrayAction::Show => {
                    let hwnd = self.hwnd.0.cast();
                    // SAFETY: the window is ours and alive for the app's lifetime.
                    unsafe {
                        ShowWindow(hwnd, SW_SHOW);
                        SetForegroundWindow(hwnd);
                    }
                }
                TrayAction::Quit => {
                    let hwnd = self.hwnd.0.cast();
                    // SAFETY: the window is ours; WM_CLOSE is the normal close path.
                    unsafe {
                        PostMessageW(hwnd, WM_CLOSE, 0, 0);
                    }
                }
                TrayAction::Restart => {
                    if let Err(error) = std::thread::Builder::new()
                        .name("service restart".to_owned())
                        .spawn(crate::service::restart)
                    {
                        tracing::error!(
                            target: "service",
                            "could not start the restart thread: {error}"
                        );
                    }
                }
            }
        }
    }

    fn present(&self, outcome: FrameOutcome) -> FrameOutcome {
        let (width, height) = self.swapchain.borrow().size();
        let index = self.swapchain.borrow().current_index();
        let Some(&target_tex) = self.targets.get(usize::try_from(index).unwrap_or(0)) else {
            return outcome;
        };
        let damaged = self.damaged.replace(false);
        if !damaged && self.presented.get() {
            return outcome;
        }
        if !self.swapchain.borrow().wait_for_frame(100) {
            // Occluded or minimized: drop this frame instead of blocking.
            self.damaged.set(true);
            return outcome;
        }

        #[allow(
            clippy::cast_precision_loss,
            reason = "window sizes are well inside f32's exact integer range"
        )]
        let (width_f, height_f) = (width as f32, height as f32);
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
        target.crop = pl::pl_rect2df {
            x0: 0.0,
            y0: 0.0,
            x1: width_f,
            y1: height_f,
        };

        let part = pl::pl_overlay_part {
            src: pl::pl_rect2df {
                x0: 0.0,
                y0: 0.0,
                x1: width_f,
                y1: height_f,
            },
            dst: pl::pl_rect2df {
                x0: 0.0,
                y0: 0.0,
                x1: width_f,
                y1: height_f,
            },
            color: [1.0, 1.0, 1.0, 1.0],
        };
        let overlay = self.ui_overlay(&part);
        target.overlays = &raw const overlay;
        target.num_overlays = 1;

        // ZGUI just finished a pass into the interface texture, so it is in
        // `COLOR_ATTACHMENT_OPTIMAL`; hand it to libplacebo and take it back.
        self.placebo.release(
            self.ui,
            pl::VkImageLayout::VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL,
        );
        if !self.placebo.render(None, &target) {
            tracing::warn!(target: "ui", "libplacebo: render failed for {width}x{height}");
        }
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
            tracing::warn!(target: "ui", "libplacebo: failed to take the interface texture back");
        }

        if let Err(error) = self.swapchain.borrow().present() {
            tracing::warn!(target: "ui", "DXGI present failed: {error:#}");
        }
        self.presented.set(true);
        outcome
    }
}

impl Drop for UiRenderer {
    fn drop(&mut self) {
        for target in &mut self.targets {
            self.placebo.destroy_texture(target);
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
        self.inner.configure(target);
    }

    fn target(&self) -> Option<RenderTarget> {
        self.inner.target()
    }

    fn draw(&mut self, scene: &Scene, damage: &DamageSet) -> FrameOutcome {
        self.sync();
        if !self.inner.present_into(0) {
            tracing::warn!(target: "ui", "the UI texture was not supplied");
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
