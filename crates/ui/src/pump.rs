//! The frame pump: asks ZGUI for a frame at a fixed UI cadence.
//!
//! The renderer drains RPC events and updates the reactive snapshot during
//! `draw`, so the pump only has to keep frames coming. A fully transparent
//! 1×1 texture takes the surface element's place and paints nothing.

use std::sync::Arc;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use std::time::Instant;

use zgui::surface::SurfaceEvent;
use zgui::surface::SurfaceHandle;
use zgui::surface::wgpu;

/// How often the UI asks for a frame.
const INTERVAL: Duration = Duration::from_millis(100);

pub fn spawn(handle: SurfaceHandle) {
    // Install the sink before the view is built: `Attached` fires when the
    // surface element mounts, and a later sink would miss the device.
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

        // Fully transparent and never written: alpha 0 means it paints nothing.
        let texture = Arc::new(gpu.device().create_texture(&wgpu::TextureDescriptor {
            label: Some("siphon.pump"),
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

        let mut due = Instant::now();
        loop {
            while let Ok(event) = receiver.try_recv() {
                if matches!(event, SurfaceEvent::Detached) {
                    return;
                }
            }
            handle.present(Arc::clone(&texture));

            due += INTERVAL;
            let now = Instant::now();
            if due > now {
                thread::sleep(due - now);
            } else {
                due = now;
            }
        }
    });
}
