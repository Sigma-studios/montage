//! Drawing the captions onto the exported frames: RGBA frames to and from
//! the overlay's packed image, and the colour matrices that make the trip
//! through RGBA match the video's.

use ffmpeg_next::ffi;
use ffmpeg_next::format::Pixel;
use ffmpeg_next::frame;
use ffmpeg_next::software::scaling;
use ffmpeg_next::util::color;
use montage_core::render::RgbaImage;

/// Copies an RGBA frame into `image` (rows packed), resizing it as needed.
pub(crate) fn frame_to_image(frame: &frame::Video, image: &mut RgbaImage) {
    debug_assert_eq!(frame.format(), Pixel::RGBA);
    let (width, height) = (frame.width(), frame.height());
    let row = width as usize * 4;
    image.width = width;
    image.height = height;
    image.data.resize(row * height as usize, 0);
    let stride = frame.stride(0);
    let data = frame.data(0);
    for (y, out) in image.data.chunks_exact_mut(row).enumerate() {
        out.copy_from_slice(&data[y * stride..y * stride + row]);
    }
}

/// Copies `image` into an RGBA frame of the same size.
pub(crate) fn image_to_frame(image: &RgbaImage, frame: &mut frame::Video) {
    debug_assert_eq!(frame.format(), Pixel::RGBA);
    debug_assert_eq!((frame.width(), frame.height()), (image.width, image.height));
    let row = image.width as usize * 4;
    let stride = frame.stride(0);
    let data = frame.data_mut(0);
    for (y, src) in image.data.chunks_exact(row).enumerate() {
        data[y * stride..y * stride + row].copy_from_slice(src);
    }
}

/// The YUV matrix of `frame`: as tagged, or by size when untagged (HD is
/// BT.709, SD BT.601), as players guess.
fn matrix(frame: &frame::Video) -> std::os::raw::c_int {
    match frame.color_space() {
        color::Space::BT709 => ffi::SWS_CS_ITU709,
        color::Space::BT470BG | color::Space::SMPTE170M => ffi::SWS_CS_ITU601,
        color::Space::BT2020NCL | color::Space::BT2020CL => ffi::SWS_CS_BT2020,
        _ if frame.height() >= 720 => ffi::SWS_CS_ITU709,
        _ => ffi::SWS_CS_ITU601,
    }
}

/// True when `frame`'s YUV uses the full 0-255 range.
fn full_range(frame: &frame::Video) -> bool {
    frame.color_range() == color::Range::JPEG
        || matches!(
            frame.format(),
            Pixel::YUVJ420P | Pixel::YUVJ422P | Pixel::YUVJ444P
        )
}

/// True for YUV formats: those with colour and no RGB flag.
fn is_yuv(format: Pixel) -> bool {
    format.descriptor().is_some_and(|d| {
        // SAFETY: the descriptor is a static table entry.
        let flags = unsafe { (*d.as_ptr()).flags };
        d.nb_components() >= 3 && flags & ffi::AV_PIX_FMT_FLAG_RGB as u64 == 0
    })
}

/// Sets `scaler`, which turns frames like `source` into RGBA, to use the
/// source's matrix and range, so the colours come out as players show
/// them.
pub(crate) fn yuv_to_rgb_colors(scaler: &mut scaling::Context, source: &frame::Video) {
    if !is_yuv(source.format()) {
        return;
    }
    set_details(scaler, matrix(source), full_range(source), true);
}

/// Sets `scaler`, which turns RGBA back into the encoder's limited-range
/// YUV, to use the matrix of `source`, the frame the RGBA came from.
pub(crate) fn rgb_to_yuv_colors(scaler: &mut scaling::Context, source: &frame::Video) {
    set_details(scaler, matrix(source), false, false);
}

fn set_details(
    scaler: &mut scaling::Context,
    matrix: std::os::raw::c_int,
    yuv_full: bool,
    to_rgb: bool,
) {
    // SAFETY: plain calls on a live context; the coefficient tables are
    // static.
    unsafe {
        let coefficients = ffi::sws_getCoefficients(matrix);
        let (src_range, dst_range) = if to_rgb {
            (i32::from(yuv_full), 1)
        } else {
            (1, i32::from(yuv_full))
        };
        let _ = ffi::sws_setColorspaceDetails(
            scaler.as_mut_ptr(),
            coefficients,
            src_range,
            coefficients,
            dst_range,
            0,
            1 << 16,
            1 << 16,
        );
    }
}
