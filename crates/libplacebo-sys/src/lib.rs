//! Raw bindings to the libplacebo this repository builds, plus its
//! header-only libav glue.
//!
//! The bindings are generated from the headers in `LIBPLACEBO_PREFIX` at build
//! time, so they always match the installed library rather than a snapshot.

#![allow(
    non_upper_case_globals,
    non_camel_case_types,
    non_snake_case,
    reason = "bindgen emits C names, and the C API is the interface"
)]
#![allow(clippy::all, clippy::pedantic)]
#![allow(
    clippy::multiple_unsafe_ops_per_block,
    clippy::borrow_as_ptr,
    clippy::ptr_as_ptr,
    clippy::ptr_cast_constness,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    clippy::cast_ptr_alignment,
    clippy::unnecessary_cast,
    clippy::use_self,
    clippy::undocumented_unsafe_blocks,
    clippy::doc_markdown,
    reason = "bindgen's declarations, not code: `Self` in a const, one SAFETY
              story per generated impl, and no docs to mark up"
)]

include!(concat!(env!("OUT_DIR"), "/bindings.rs"));

/// `libplacebo/utils/libav.h`: map libav `AVFrame`s (including `AV_PIX_FMT_VULKAN`
/// hw frames, with their `AVVkFrame` semaphores) to `pl_frame`s.
///
/// `AVFrame` is opaque here: pass the pointer from your libav bindings.
pub mod libav {
    use super::pl_frame;
    use super::pl_gpu;
    use super::pl_tex;
    use std::os::raw::c_void;

    /// `struct pl_avframe_params`.
    #[repr(C)]
    #[derive(Debug, Clone, Copy)]
    pub struct pl_avframe_params {
        /// `const AVFrame *`. Required.
        pub frame: *const c_void,
        /// Four reusable textures, required for software frames; may be null for hw frames.
        pub tex: *mut pl_tex,
        pub map_dovi: bool,
        pub map_dovi_force: bool,
    }

    unsafe extern "C" {
        pub fn pl_map_avframe_ex(
            gpu: pl_gpu,
            out_frame: *mut pl_frame,
            params: *const pl_avframe_params,
        ) -> bool;
        pub fn pl_unmap_avframe(gpu: pl_gpu, frame: *mut pl_frame);
        /// Fills colour/repr/planes metadata (not textures) from an `AVFrame`.
        pub fn pl_frame_from_avframe(out_frame: *mut pl_frame, frame: *const c_void);
        /// The `AVFrame *` a frame was mapped from.
        pub fn pl_get_mapped_avframe(frame: *const pl_frame) -> *mut c_void;
    }
}
