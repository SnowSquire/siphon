//! One file, one decoder, and the frames it produces.
//!
//! The decoder runs on the device wgpu owns (see `hw`), so its output frames are
//! `VkImage`s libplacebo can sample directly. `pl_map_avframe_ex` turns each one
//! into a `pl_frame`, including the semaphores FFmpeg's queue signals after the
//! decode, so there is no copy back to the host anywhere.

use std::path::Path;

use anyhow::Context as _;
use ffmpeg_the_third::codec;
use ffmpeg_the_third::ffi;
use ffmpeg_the_third::format;
use ffmpeg_the_third::frame;
use ffmpeg_the_third::media::Type;
use libplacebo_sys as pl;
use zgui_render_wgpu::Gpu;

use crate::gpu::VideoDecode;
use crate::hw;
use crate::hw::choose_vulkan_format;

pub struct Player {
    input: format::context::Input,
    stream_index: usize,
    decoder: codec::decoder::Video,
    /// The frame `decode` last produced, moved out on the next call.
    frame: frame::Video,
    /// The stream's time base, for turning `pts` into seconds.
    time_base: f64,
    /// The container's colour description, applied to frames the decoder leaves
    /// unspecified: HEVC's VUI wins over the container, and this file's VUI says
    /// nothing while the container says BT.2020/PQ.
    color: codec::Parameters,
    /// Keeps the decoder's device reference alive for the whole stream.
    _device: hw::HwDevice,
    eof: bool,
}

impl Player {
    pub fn open(gpu: &Gpu, video: VideoDecode, path: &Path) -> anyhow::Result<Self> {
        let device = hw::HwDevice::new(gpu, video)?;
        let input = format::input(path).with_context(|| format!("open {}", path.display()))?;
        let (stream_index, mut context, time_base, params) = {
            let stream = input
                .streams()
                .best(Type::Video)
                .context("no video stream")?;
            let rate = stream.time_base();
            let time_base = f64::from(rate.numerator()) / f64::from(rate.denominator());
            let params = copy_parameters(&stream.parameters())?;
            (
                stream.index(),
                codec::context::Context::from_parameters(stream.parameters())?,
                time_base,
                params,
            )
        };
        // One decode thread: the packet path runs on the UI thread, and the
        // hardware does the work.
        // SAFETY: the context is not open yet and the fields are set before
        // `decoder().video()` opens it. FFmpeg retains the device reference.
        unsafe {
            let raw = context.as_mut_ptr();
            (*raw).hw_device_ctx = ffi::av_buffer_ref(device.as_ptr());
            (*raw).get_format = Some(choose_vulkan_format);
            (*raw).thread_count = 1;
        }
        let decoder = context.decoder().video()?;
        Ok(Self {
            input,
            stream_index,
            decoder,
            frame: frame::Video::empty(),
            time_base,
            color: params,
            _device: device,
            eof: false,
        })
    }

    /// Decodes the next frame and hands it over.
    ///
    /// Returns `None` once the stream ends.
    pub fn decode(&mut self) -> anyhow::Result<Option<frame::Video>> {
        if self.eof {
            return Ok(None);
        }
        loop {
            if self.decoder.receive_frame(&mut self.frame).is_ok() {
                self.describe();
                return Ok(Some(std::mem::replace(
                    &mut self.frame,
                    frame::Video::empty(),
                )));
            }
            if self.eof {
                return Ok(None);
            }
            if let Some(packet) = self.input.packets().next() {
                let (stream, packet) = packet?;
                if stream.index() == self.stream_index {
                    self.decoder.send_packet(&packet)?;
                }
            } else {
                self.decoder.send_eof()?;
                self.eof = true;
            }
        }
    }

    /// Fills in the colour description the decoder left unspecified.
    fn describe(&mut self) {
        // SAFETY: the frame belongs to this decoder and is live.
        unsafe {
            let frame = self.frame.as_mut_ptr();
            // SAFETY: the parameters reference is alive next to the decoder.
            let params = self.color.as_ptr();
            if (*frame).color_trc.0 == ffi::AVColorTransferCharacteristic::UNSPECIFIED.0 {
                (*frame).color_trc = (*params).color_trc;
            }
            if (*frame).color_primaries.0 == ffi::AVColorPrimaries::UNSPECIFIED.0 {
                (*frame).color_primaries = (*params).color_primaries;
            }
            if (*frame).colorspace.0 == ffi::AVColorSpace::UNSPECIFIED.0 {
                (*frame).colorspace = (*params).color_space;
            }
            if (*frame).color_range.0 == ffi::AVColorRange::UNSPECIFIED.0 {
                (*frame).color_range = (*params).color_range;
            }
            if (*frame).chroma_location.0 == ffi::AVChromaLocation::UNSPECIFIED.0 {
                (*frame).chroma_location = (*params).chroma_location;
            }
        }
    }

    /// Where the frame sits in the stream, in seconds.
    pub fn timestamp(&self, frame: &frame::Video) -> f64 {
        #[allow(
            clippy::cast_precision_loss,
            reason = "stream timestamps are far below f64's exact range"
        )]
        frame.pts().map_or(0.0, |pts| pts as f64 * self.time_base)
    }
}

/// Takes its own reference to a stream's codec parameters.
///
/// The stream is borrowed for the whole file, but the reference makes the
/// colour fields outlive any borrow of the demuxer.
fn copy_parameters(
    params: &codec::parameters::ParametersRef<'_>,
) -> anyhow::Result<codec::Parameters> {
    // SAFETY: `avcodec_parameters_alloc` returns an owned reference, and
    // `avcodec_parameters_copy` fills it from a live source.
    unsafe {
        let copy = ffi::avcodec_parameters_alloc();
        anyhow::ensure!(!copy.is_null(), "avcodec_parameters_alloc failed");
        let code = ffi::avcodec_parameters_copy(copy, params.as_ptr());
        if code < 0 {
            let mut failed = copy;
            ffi::avcodec_parameters_free(&raw mut failed);
            anyhow::bail!("avcodec_parameters_copy failed: {code}");
        }
        codec::Parameters::from_raw(copy).context("avcodec_parameters_alloc returned no parameters")
    }
}

/// A `pl_frame` mapped from an `AVFrame`, unmapped on drop.
pub struct MappedFrame {
    /// Must outlive `frame`: the plane textures wrap this AVFrame's images.
    _picture: frame::Video,
    frame: pl::pl_frame,
    gpu: pl::pl_gpu,
}

impl MappedFrame {
    pub fn map(gpu: pl::pl_gpu, picture: &frame::Video) -> anyhow::Result<Self> {
        // The mapping keeps its own reference to the AVFrame; cloning is cheap
        // (a refcount bump) and keeps the images alive for the textures below.
        let mut owned = frame::Video::empty();
        // SAFETY: both frames are live; `av_frame_ref` takes a new reference.
        unsafe {
            let code = ffi::av_frame_ref(owned.as_mut_ptr(), picture.as_ptr());
            anyhow::ensure!(code >= 0, "av_frame_ref failed: {code}");
        }

        let mut mapped = pl::pl_frame::default();
        let params = pl::libav::pl_avframe_params {
            // SAFETY: `owned` is a live AVFrame for the whole mapping.
            frame: unsafe { owned.as_ptr() }.cast(),
            tex: std::ptr::null_mut(),
            map_dovi: false,
            map_dovi_force: false,
        };
        // SAFETY: the AVFrame is live for the call and the mapped frame owns
        // everything `pl_map_avframe_ex` allocated.
        if unsafe { !pl::libav::pl_map_avframe_ex(gpu, &mut mapped, &params) } {
            anyhow::bail!("libplacebo refused to map the decoded frame");
        }
        Ok(Self {
            _picture: owned,
            frame: mapped,
            gpu,
        })
    }

    pub const fn frame(&self) -> &pl::pl_frame {
        &self.frame
    }

    /// Tells libplacebo where the display's SDR white sits.
    ///
    /// Only for sources that are not HDR themselves: a PQ frame already carries
    /// absolute luminance, while an SDR one is relative to the display's white.
    pub fn set_sdr_white(&mut self, nits: f32) {
        // SAFETY: the frame is live for the borrow.
        if !unsafe { pl::pl_color_space_is_hdr(&self.frame.color) } {
            self.frame.color.hdr.max_luma = nits;
        }
    }
}

impl Drop for MappedFrame {
    fn drop(&mut self) {
        // SAFETY: the frame was mapped by `pl_map_avframe_ex` and is unmapped once.
        unsafe { pl::libav::pl_unmap_avframe(self.gpu, &raw mut self.frame) };
    }
}
