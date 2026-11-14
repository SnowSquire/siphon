//! A generated test pattern presented through the shared wgpu device.
//!
//! This is the producer half of ZGUI's `surface` element: a thread with its own
//! cadence creates textures on the device the element hands it and presents the
//! newest one. The renderer never reads pixels back.

use std::sync::Arc;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use zgui::surface::SurfaceEvent;
use zgui::surface::SurfaceHandle;
use zgui::surface::SurfaceIntrinsic;
use zgui::surface::wgpu;

pub fn spawn(handle: SurfaceHandle) {
    handle.set_intrinsic(SurfaceIntrinsic::ratio(16.0 / 9.0));

    let (events, receiver) = mpsc::channel();
    handle.set_events(move |event| {
        let _ = events.send(event);
    });

    thread::spawn(move || {
        // Wait until the element is laid out; `Attached` is what delivers the
        // device the textures must live on.
        let (gpu, mut size) = loop {
            match receiver.recv() {
                Ok(SurfaceEvent::Attached { gpu, size, .. }) => break (gpu, size),
                Ok(_) => {}
                Err(_) => return,
            }
        };

        let mut frame = 0u64;
        let mut visible = true;
        loop {
            while let Ok(event) = receiver.try_recv() {
                match event {
                    SurfaceEvent::Resized { size: new, .. } => size = new,
                    SurfaceEvent::Visible(now) => visible = now,
                    SurfaceEvent::Detached => return,
                    _ => {}
                }
            }

            thread::sleep(Duration::from_millis(33));
            if !visible || size.width == 0 || size.height == 0 {
                continue;
            }

            // A fresh texture per present keeps ownership obvious. A real
            // player will round-robin two or three and gate reuse on the GPU.
            let texture = gpu.device().create_texture(&wgpu::TextureDescriptor {
                label: Some("video.pattern"),
                size: wgpu::Extent3d {
                    width: size.width,
                    height: size.height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING
                    | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });

            gpu.queue().write_texture(
                texture.as_image_copy(),
                &gradient(size.width, size.height, frame),
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(size.width * 4),
                    rows_per_image: None,
                },
                wgpu::Extent3d {
                    width: size.width,
                    height: size.height,
                    depth_or_array_layers: 1,
                },
            );

            handle.present(Arc::new(texture));
            frame += 1;
        }
    });
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]
fn gradient(width: u32, height: u32, frame: u64) -> Vec<u8> {
    let capacity = usize::try_from(u64::from(width) * u64::from(height) * 4).unwrap_or(0);
    let mut texels = Vec::with_capacity(capacity);
    let shift = u32::try_from(frame.wrapping_mul(3)).unwrap_or(0);
    for y in 0..height {
        for x in 0..width {
            let across = (x.wrapping_add(shift) % width.max(1)) as f32 / width.max(1) as f32;
            let down = y as f32 / height.max(1) as f32;
            texels.extend([(across * 255.0) as u8, (down * 160.0) as u8, 180, 255]);
        }
    }
    texels
}
