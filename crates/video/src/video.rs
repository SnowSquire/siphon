//! A very small file player: decode the first video stream and present the frames
//! through a ZGUI surface.
//!
//! This stage is deliberately synchronous and software-decoded. The Vulkan device
//! adoption and hardware decode replace the decode half later; the presentation
//! half (RGBA texture on the shared device) stays the same.

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use anyhow::Context as _;
use ffmpeg_the_third as ffmpeg;
use ffmpeg::format;
use ffmpeg::frame;
use ffmpeg::media::Type;
use ffmpeg::software::scaling;
use zgui::surface::SurfaceEvent;
use zgui::surface::SurfaceHandle;
use zgui::surface::SurfaceIntrinsic;
use zgui::surface::wgpu;

pub fn spawn(handle: SurfaceHandle, path: PathBuf) {
    // Install the event sink before the view is built. `Attached` is emitted
    // once, when the surface element is mounted; a sink installed after that
    // would miss it and wait forever for a device that already exists.
    let (events, receiver) = mpsc::channel();
    handle.set_events(move |event| {
        let _ = events.send(event);
    });
    handle.set_intrinsic(SurfaceIntrinsic::ratio(16.0 / 9.0));

    thread::spawn(move || {
        if let Err(error) = play(&handle, &receiver, &path) {
            eprintln!("video: {error:#}");
        }
    });
}

fn play(
    handle: &SurfaceHandle,
    receiver: &mpsc::Receiver<SurfaceEvent>,
    path: &Path,
) -> anyhow::Result<()> {
    let mut input = format::input(path).with_context(|| format!("open {}", path.display()))?;
    let (stream_index, mut decoder, frame_rate) = {
        let stream = input
            .streams()
            .best(Type::Video)
            .context("the file has no video stream")?;
        let decoder =
            ffmpeg::codec::context::Context::from_parameters(stream.parameters())?.decoder().video()?;
        (stream.index(), decoder, stream.avg_frame_rate())
    };

    let width = decoder.width();
    let height = decoder.height();
    #[allow(clippy::cast_precision_loss)]
    handle.set_intrinsic(SurfaceIntrinsic::size(width as f32, height as f32));

    // Wait for the element to be mounted so the device and its epoch are known.
    let mut visible = true;
    let mut gpu = loop {
        match receiver.recv() {
            Ok(SurfaceEvent::Attached { gpu, .. }) => break gpu,
            Ok(SurfaceEvent::Visible(now)) => visible = now,
            Ok(SurfaceEvent::Detached) | Err(_) => return Ok(()),
            Ok(_) => {}
        }
    };

    let interval = frame_interval(frame_rate);
    let mut scaler = None;
    let mut picture = frame::Video::empty();
    let mut rgba = frame::Video::empty();

    for packet in input.packets() {
        let (stream, packet) = packet?;
        if stream.index() != stream_index {
            continue;
        }
        decoder.send_packet(&packet)?;
        while decoder.receive_frame(&mut picture).is_ok() {
            drain_events(receiver, &mut gpu, &mut visible);
            if !visible {
                continue;
            }
            present(handle, &gpu, &picture, &mut scaler, &mut rgba)?;
            thread::sleep(interval);
        }
    }

    decoder.send_eof()?;
    while decoder.receive_frame(&mut picture).is_ok() {
        drain_events(receiver, &mut gpu, &mut visible);
        if visible {
            present(handle, &gpu, &picture, &mut scaler, &mut rgba)?;
            thread::sleep(interval);
        }
    }

    Ok(())
}

fn drain_events(
    receiver: &mpsc::Receiver<SurfaceEvent>,
    gpu: &mut zgui::surface::GpuShare,
    visible: &mut bool,
) {
    while let Ok(event) = receiver.try_recv() {
        match event {
            SurfaceEvent::DeviceLost { gpu: replacement } => *gpu = replacement,
            SurfaceEvent::Visible(now) => *visible = now,
            SurfaceEvent::Detached => *visible = false,
            _ => {}
        }
    }
}

fn present(
    handle: &SurfaceHandle,
    gpu: &zgui::surface::GpuShare,
    decoded: &frame::Video,
    scaler: &mut Option<scaling::Context>,
    rgba: &mut frame::Video,
) -> anyhow::Result<()> {
    let width = decoded.width();
    let height = decoded.height();
    let source_format = decoded.format();
    let needs_scaler = scaler.as_ref().is_none_or(|current| {
        current.input().format != source_format
            || current.input().width != width
            || current.input().height != height
    });
    if needs_scaler {
        *scaler = Some(scaling::Context::get(
            source_format,
            width,
            height,
            format::Pixel::RGBA,
            width,
            height,
            scaling::Flags::BILINEAR,
        )?);
    }
    let scaler = scaler.as_mut().expect("just built");
    scaler.run(decoded, rgba)?;

    let row = usize::try_from(width).unwrap_or(0) * 4;
    let stride = rgba.stride(0);
    let padded_row = row.div_ceil(256) * 256;
    let mut packed = vec![0u8; padded_row * usize::try_from(height).unwrap_or(0)];
    for line in 0..usize::try_from(height).unwrap_or(0) {
        packed[line * padded_row..line * padded_row + row]
            .copy_from_slice(&rgba.data(0)[line * stride..line * stride + row]);
    }

    let texture = gpu.device().create_texture(&wgpu::TextureDescriptor {
        label: Some("video.frame"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    gpu.queue().write_texture(
        texture.as_image_copy(),
        &packed,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(u32::try_from(padded_row).unwrap_or(0)),
            rows_per_image: None,
        },
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );
    handle.present(Arc::new(texture));
    Ok(())
}

fn frame_interval(rate: ffmpeg::Rational) -> Duration {
    if rate.numerator() == 0 || rate.denominator() == 0 {
        return Duration::from_millis(33);
    }
    Duration::from_secs_f64(f64::from(rate.denominator()) / f64::from(rate.numerator()))
}
