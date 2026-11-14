//! The frame pump: what asks ZGUI for a frame once per video frame.
//!
//! The renderer decodes and presents one frame each time the runtime runs one, so
//! something has to ask at the video's rate. That is a surface element holding a
//! fully transparent texture, presented once per frame interval. It takes the
//! preview box's place in the layout and paints nothing, which is exactly right:
//! the picture is underneath, drawn by libplacebo.

use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use std::time::Instant;

use ffmpeg_the_third as ffmpeg;
use ffmpeg::format;
use ffmpeg::media::Type;
use zgui::surface::SurfaceEvent;
use zgui::surface::SurfaceHandle;
use zgui::surface::wgpu;

/// The video's frame rate, or 30 when the file does not say.
pub fn frame_rate(path: &Path) -> f64 {
    let rate = format::input(path)
        .ok()
        .and_then(|input| {
            input
                .streams()
                .best(Type::Video)
                .map(|stream| stream.avg_frame_rate())
        })
        .filter(|rate| rate.numerator() > 0 && rate.denominator() > 0)
        .map_or(0.0, |rate| {
            f64::from(rate.numerator()) / f64::from(rate.denominator())
        });
    if rate > 0.0 { rate } else { 30.0 }
}

pub fn spawn(handle: SurfaceHandle, fps: f64) {
    // Install the event sink before the view is built. `Attached` is emitted
    // once, when the surface element is mounted; a sink installed after that
    // would miss it and wait forever for a device that already exists.
    let (events, receiver) = mpsc::channel();
    handle.set_events(move |event| {
        let _ = events.send(event);
    });

    thread::spawn(move || {
        let gpu = loop {
            match receiver.recv() {
                Ok(SurfaceEvent::Attached { gpu, .. }) => break gpu,
                Ok(SurfaceEvent::Detached) | Err(_) => return,
                Ok(_) => {}
            }
        };

        // Fully transparent and never written, so wgpu's zero initialisation is
        // all it needs: alpha 0 means the element paints nothing.
        let texture = Arc::new(gpu.device().create_texture(&wgpu::TextureDescriptor {
            label: Some("video.pump"),
            size: wgpu::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        }));

        let interval = Duration::from_secs_f64(1.0 / fps.max(1.0));
        let mut due = Instant::now();
        loop {
            while let Ok(event) = receiver.try_recv() {
                if matches!(event, SurfaceEvent::Detached) {
                    return;
                }
            }
            handle.present(Arc::clone(&texture));

            due += interval;
            let now = Instant::now();
            if due > now {
                thread::sleep(due - now);
            } else {
                due = now;
            }
        }
    });
}

