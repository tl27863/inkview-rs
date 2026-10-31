use crate::bindings::{
    APPLICATION_ATTRIBUTE_APPLICATION_READER, EVT_CUSTOM, PANEL_FLAGS_PANEL_NO_FB_OFFSET,
};
use crate::error;
use crate::{bindings::icanvas_s, bindings::Inkview};
use core::ffi::c_int;
use core::fmt::Display;
use num_traits::{FromPrimitive, ToPrimitive};
use std::marker::PhantomData;
use std::ptr::NonNull;

/// A pixel in the task framebuffer's native layout.
///
/// # Safety
///
/// Implementors must be exactly `BYTES` bytes with alignment 1 and no padding,
/// so a slice of them can be copied into the framebuffer as raw bytes
/// (see [`Screen::copy_rect`]).
pub unsafe trait PixelFormat: Sized + Copy {
    /// Bytes per pixel in the framebuffer.
    const BYTES: usize;

    fn to_bb8(&self) -> BB8;
    fn to_rgb24(&self) -> RGB24;
    fn from_rgb24(r: u8, g: u8, b: u8) -> Self;

    /// `self * by / 255 + other` per channel, saturating at 255.
    ///
    /// This is source-over blending when `other` is a premultiplied color and
    /// `by` is `255 - alpha`.
    fn mix(&self, other: Self, by: u8) -> Self;
}

/// 8-bit grayscale pixel.
#[repr(transparent)]
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug, Default)]
pub struct BB8(pub u8);

/// 24-bit pixel, stored as three bytes in the order red, green, blue.
#[repr(C)]
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug, Default)]
pub struct RGB24(pub u8, pub u8, pub u8);

#[inline(always)]
fn mix_channel(dst: u8, src: u8, by: u8) -> u8 {
    (dst as u16 * by as u16 / 255 + src as u16).min(255) as u8
}

// SAFETY: `repr(transparent)` over `u8`.
unsafe impl PixelFormat for BB8 {
    const BYTES: usize = 1;

    fn to_bb8(&self) -> BB8 {
        *self
    }

    fn to_rgb24(&self) -> RGB24 {
        RGB24(self.0, self.0, self.0)
    }

    fn from_rgb24(r: u8, g: u8, b: u8) -> Self {
        RGB24(r, g, b).to_bb8()
    }

    fn mix(&self, other: Self, by: u8) -> Self {
        Self(mix_channel(self.0, other.0, by))
    }
}

// SAFETY: `repr(C)` with three `u8` fields: 3 bytes, alignment 1, no padding.
unsafe impl PixelFormat for RGB24 {
    const BYTES: usize = 3;

    /// Rec. 709 luma in integer arithmetic; neutral grays map to themselves.
    fn to_bb8(&self) -> BB8 {
        let y = 54 * self.0 as u32 + 183 * self.1 as u32 + 19 * self.2 as u32;
        BB8((y >> 8) as u8)
    }

    fn to_rgb24(&self) -> RGB24 {
        *self
    }

    fn from_rgb24(r: u8, g: u8, b: u8) -> Self {
        Self(r, g, b)
    }

    fn mix(&self, other: Self, by: u8) -> Self {
        Self(
            mix_channel(self.0, other.0, by),
            mix_channel(self.1, other.1, by),
            mix_channel(self.2, other.2, by),
        )
    }
}

pub struct Screen<'a, P: 'static> {
    iv: &'a Inkview,

    #[allow(unused)]
    fb: &'a mut icanvas_s,
    width: usize,
    height: usize,
    /// Bytes per framebuffer row.
    stride: usize,
    buf: *mut u8,
    task: c_int,

    dpi: u32,
    scale: f32,

    _phantom: PhantomData<&'static P>,
}

pub enum WhichScreen<'a> {
    BB8(Screen<'a, BB8>),
    RGB24(Screen<'a, RGB24>),
}

impl<'a> WhichScreen<'a> {
    /// Opens the task framebuffer and picks the pixel format from its depth.
    ///
    /// Panics for depths other than 8 and 24 bits.
    pub fn new(iv: &'a Inkview) -> Self {
        let (fb, task) = open_framebuffer(iv);
        // SAFETY: InkView owns the framebuffer for the life of the task and
        // the screen holds the only Rust reference to it.
        let fb = unsafe { &mut *fb.as_ptr() };
        match fb.depth {
            8 => WhichScreen::BB8(Screen::from_framebuffer(iv, fb, task)),
            24 => WhichScreen::RGB24(Screen::from_framebuffer(iv, fb, task)),
            depth => panic!("unsupported framebuffer depth: {depth} bits (expected 8 or 24)"),
        }
    }
}

/// Prepares the application and returns the task framebuffer.
fn open_framebuffer(iv: &Inkview) -> (NonNull<icanvas_s>, c_int) {
    unsafe {
        iv.SetCurrentApplicationAttribute(APPLICATION_ATTRIBUTE_APPLICATION_READER, 1);
    }
    unsafe {
        iv.SetPanelType(PANEL_FLAGS_PANEL_NO_FB_OFFSET as c_int);
    }
    let task = unsafe { iv.GetCurrentTask() };
    // On the emulator the task framebuffer is null, so fall back to the global
    // canvas (GetCanvas) — the same `icanvas_s`, populated in both environments.
    // We do this with a plain null-check so it also recovers if a real task
    // ever hands back a null framebuffer.
    let fb = unsafe {
        let task_fb = iv.GetTaskFramebuffer(task);
        if task_fb.is_null() {
            iv.GetCanvas()
        } else {
            task_fb
        }
    };
    let fb = NonNull::new(fb)
        .expect("Failed to get a framebuffer (task framebuffer and GetCanvas both null).");
    (fb, task)
}

impl<'a, P: PixelFormat> Screen<'a, P> {
    /// Opens the task framebuffer as pixel format `P`.
    ///
    /// Panics if the framebuffer rows are too short for `P`, e.g. when asking
    /// for `RGB24` on an 8-bit panel. Use [`WhichScreen::new`] to pick `P`
    /// from the framebuffer depth.
    pub fn new(iv: &'a Inkview) -> Self {
        let (fb, task) = open_framebuffer(iv);
        // SAFETY: see `WhichScreen::new`.
        let fb = unsafe { &mut *fb.as_ptr() };
        Self::from_framebuffer(iv, fb, task)
    }

    fn from_framebuffer(iv: &'a Inkview, fb: &'a mut icanvas_s, task: c_int) -> Self {
        let dpi = unsafe { iv.get_screen_dpi() as u32 };
        let scale = unsafe { iv.get_screen_scale_factor() as f32 };

        let width = fb.width as usize;
        let height = fb.height as usize;
        let stride = fb.scanline as usize;
        let buf = fb.addr;

        eprintln!(
            "[inkview] framebuffer {width}x{height} depth={} stride={stride} dpi={dpi} scale={scale}",
            fb.depth
        );
        assert!(
            stride >= width * P::BYTES,
            "framebuffer row of {stride} bytes is too short for {width} pixels of {} bytes",
            P::BYTES
        );

        Screen {
            iv,
            fb,
            width,
            height,
            stride,
            buf,
            task,
            dpi,
            scale,
            _phantom: PhantomData,
        }
    }

    /// The task framebuffer as bytes.
    fn framebuffer(&mut self) -> FrameBytes<'_> {
        FrameBytes {
            // SAFETY: the task framebuffer holds `height` rows of `stride`
            // bytes and lives as long as the screen.
            bytes: unsafe { core::slice::from_raw_parts_mut(self.buf, self.stride * self.height) },
            stride: self.stride,
            width: self.width,
            height: self.height,
        }
    }

    #[inline(always)]
    pub fn draw(&mut self, x: usize, y: usize, c: P) {
        self.framebuffer()
            .copy_rect(x, y, 1, 1, core::slice::from_ref(&c), 1, (x, y));
    }

    /// Copy a rectangle of pixels into the framebuffer, one row at a time.
    ///
    /// `src` holds rows of `src_stride` pixels; the rectangle is read from the
    /// same `(x, y)` offset in `src` as it is written to in the framebuffer.
    /// The rectangle is clipped to the screen and to `src`.
    pub fn copy_rect(
        &mut self,
        x: usize,
        y: usize,
        w: usize,
        h: usize,
        src: &[P],
        src_stride: usize,
    ) {
        self.framebuffer()
            .copy_rect(x, y, w, h, src, src_stride, (0, 0));
    }
}

/// A framebuffer of `height` rows of `stride` bytes, `width` pixels each.
struct FrameBytes<'a> {
    bytes: &'a mut [u8],
    stride: usize,
    width: usize,
    height: usize,
}

impl FrameBytes<'_> {
    /// Copies the `w`×`h` rectangle at `(x, y)` from `src` (rows of
    /// `src_stride` pixels whose first pixel is the framebuffer pixel
    /// `src_origin`), clipped to the framebuffer and to `src`.
    #[allow(clippy::too_many_arguments)]
    fn copy_rect<P: PixelFormat>(
        &mut self,
        x: usize,
        y: usize,
        w: usize,
        h: usize,
        src: &[P],
        src_stride: usize,
        src_origin: (usize, usize),
    ) {
        let (ox, oy) = src_origin;
        let x_end = (x + w).min(self.width).min(ox + src_stride);
        let y_end = (y + h).min(self.height);
        if x >= x_end || x < ox || y < oy {
            return;
        }
        let len = x_end - x;

        for row in y..y_end {
            let src_start = (row - oy) * src_stride + (x - ox);
            let Some(src_row) = src.get(src_start..src_start + len) else {
                return;
            };
            // SAFETY: `PixelFormat` guarantees `P` is `BYTES` plain bytes.
            let src_bytes = unsafe {
                core::slice::from_raw_parts(src_row.as_ptr().cast::<u8>(), len * P::BYTES)
            };
            let dst_start = row * self.stride + x * P::BYTES;
            // `width * BYTES <= stride` is checked when the screen is created,
            // so this slice stays within the row.
            self.bytes[dst_start..dst_start + len * P::BYTES].copy_from_slice(src_bytes);
        }
    }
}

impl<'a, P> Screen<'a, P> {
    /// Posts a no-op `EVT_CUSTOM` to this task's InkView event queue.
    ///
    /// Screen updates requested from a thread other than the one running
    /// `InkViewMain` can sit unprocessed until that loop wakes up, which may
    /// only happen on the next input event. Posting an event wakes it. The
    /// event is not mapped by `Event::from_raw`, so it is never forwarded.
    ///
    /// Returns `SendEventTo`'s result, or `None` if the symbol is missing.
    pub fn wake_event_loop(&self) -> Option<c_int> {
        let send = self.iv.SendEventTo.as_ref().ok()?;
        Some(unsafe { send(self.task, EVT_CUSTOM as c_int, 0, 0) })
    }

    /// Block until the current panel update has finished.
    ///
    /// Only use this for infrequent transitions (show/resume or a large
    /// loading/card change); it can block for the duration of an e-ink update.
    pub fn wait_for_update_complete(&self) {
        unsafe {
            self.iv.WaitForUpdateComplete();
        }
    }

    pub fn clear(&mut self) {
        unsafe {
            self.iv.ClearScreen();
        }
    }

    pub fn full_update(&mut self) {
        unsafe {
            self.iv.FullUpdate();
        }
    }

    pub fn fast_update(&mut self) {
        unsafe {
            self.iv.SoftUpdate();
        }
    }

    // The internal partial update function.
    //
    // flags:
    //
    // PartialUpdate:         0,    0 -- normal high quality non-flashing update
    // PartialUpdateTextPage: 0xed, 0
    // PartialUpdateBlack:    0xee, 0
    // PartialUpdateDU4:      0xe5, 0
    // PartialUpdateDU4:      0xe5, 0
    // DyanmicUpdate:         0,    0
    // DyanmicUpdateBW:       1,    2
    // DyanmicUpdateA2:       0xe6, 0 and 0xef, 1
    // ExitUpdateE2:          0xe7, 0
    // #[cfg(any(feature = "sdk-5-19", feature = "sdk-6-5", feature = "sdk-6-8"))]
    // pub fn do_partial_update(&mut self, x: i32, y: i32, w: u32, h: u32, flags: u32, dynamic: bool) {
    //     unsafe {
    //         self.iv.do_partial_update(
    //             x,
    //             y,
    //             w as i32,
    //             h as i32,
    //             flags as i32,
    //             if dynamic { 1 } else { 0 },
    //         );
    //     }
    // }
    //
    /// High res partial update
    pub fn partial_update(&mut self, x: i32, y: i32, w: u32, h: u32) {
        unsafe {
            // PartialUpdate:      0,    0 -- normal high quality non-flashing update
            // PartialUpdateBlack: 0xee, 0 -- seems to do a full flash
            // PartialUpdateBW:    1,    0 -- not really any noticeable difference from 0,0
            // PartialUpdateHQ:    0xe9, 0 -- also high quality? maybe forces it?
            // PartialUpdateDU4:   0xe5, 0 -- dithered, seems to corrupt often
            // DynamicUpdate:      0,    0
            // DynamicUpdateBW:    1,    2
            // DynamicUpdateA2:    0xe6, 0  and 0xef, 1
            // ExitUpdateA2:       0xe7, 0  -- cancels an update?
            //
            //
            // tested for flag values:
            //   0xe7 -- cancels
            //   0x8000 -- dithered, flashes aswell
            //   0xeb -- normal?
            //   0xef -- dithered, no flash
            //   0xe6 -- dithered, does a quick flash?
            //   0xed -- normal?
            //   0xe5 -- dithered, corrupt
            //   0xe6 -> 0xf0
            //   0xe8 -- normal?
            //   0xea -- normal?
            //   0xec -- normal?
            //
            //
            //self.iv.do_partial_update(x, y, w as i32, h as i32, 0, 2);
            self.iv.PartialUpdate(x, y, w as i32, h as i32);
        }
    }

    /// Fast but ugly
    pub fn dynamic_update(&mut self, x: i32, y: i32, w: u32, h: u32) {
        unsafe {
            //self.iv.do_partial_update(x, y, w as i32, h as i32, 1, 2);
            self.iv.DynamicUpdate(x, y, w as i32, h as i32);
        }
    }

    pub fn is_updating(&mut self) -> bool {
        unsafe { self.iv.IsUpdateInProcess() != 0 }
    }

    /// Width of the physical screen (independent of the orientation)
    pub fn width(&self) -> usize {
        self.width
    }

    /// Height of the physical screen (independent of the orientation)
    pub fn height(&self) -> usize {
        self.height
    }

    /// Current screen orientation
    pub fn orientation(&self) -> ScreenOrientation {
        ScreenOrientation::from_iv(unsafe { self.iv.GetOrientation() })
    }

    /// Set the current screen orientation
    pub fn set_orientation(&mut self, orientation: ScreenOrientation) {
        unsafe { self.iv.SetOrientation(orientation.to_iv()) }
    }

    /// DPI of screen
    pub fn dpi(&self) -> u32 {
        self.dpi
    }

    /// Scale factor of screen
    pub fn scale(&self) -> f32 {
        self.scale
    }
}

#[derive(
    Debug, Clone, Copy, PartialEq, PartialOrd, num_derive::FromPrimitive, num_derive::ToPrimitive,
)]
pub enum ScreenOrientation {
    Portrait0Deg = 0,
    Landscape90Deg,
    Portrait180Deg,
    Landscape270Deg,
}

impl Default for ScreenOrientation {
    fn default() -> Self {
        Self::Portrait0Deg
    }
}

impl Display for ScreenOrientation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ScreenOrientation::Portrait0Deg => write!(f, "Portrait0Deg"),
            ScreenOrientation::Landscape90Deg => write!(f, "Landscape90Deg"),
            ScreenOrientation::Portrait180Deg => write!(f, "Portrait180Deg"),
            ScreenOrientation::Landscape270Deg => write!(f, "Landscape270Deg"),
        }
    }
}

impl TryFrom<i32> for ScreenOrientation {
    type Error = error::EnumPrimitiveConversionError<i32>;

    fn try_from(value: i32) -> Result<Self, Self::Error> {
        Self::from_i32(value).ok_or_else(|| error::EnumPrimitiveConversionError {
            src_t: "i32".to_string(),
            dest_t: "ScreenOrientation".to_string(),
            value,
        })
    }
}

impl TryFrom<ScreenOrientation> for i32 {
    type Error = error::EnumPrimitiveConversionError<ScreenOrientation>;

    fn try_from(value: ScreenOrientation) -> Result<Self, Self::Error> {
        value
            .to_i32()
            .ok_or_else(|| error::EnumPrimitiveConversionError {
                src_t: "ScreenOrientation".to_string(),
                dest_t: "i32".to_string(),
                value,
            })
    }
}

impl ScreenOrientation {
    fn from_iv(raw: c_int) -> Self {
        match raw {
            0 => Self::Portrait0Deg,
            1 => Self::Landscape270Deg,
            2 => Self::Landscape90Deg,
            3 => Self::Portrait180Deg,
            raw => panic!("ScreenOrientation from inkview enum invalid num: {raw}"),
        }
    }

    fn to_iv(self) -> c_int {
        match self {
            ScreenOrientation::Portrait0Deg => 0,
            ScreenOrientation::Landscape90Deg => 2,
            ScreenOrientation::Portrait180Deg => 3,
            ScreenOrientation::Landscape270Deg => 1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pixel_sizes_match_bytes() {
        assert_eq!(size_of::<BB8>(), BB8::BYTES);
        assert_eq!(size_of::<RGB24>(), RGB24::BYTES);
        assert_eq!(align_of::<RGB24>(), 1);
    }

    #[test]
    fn rgb24_to_bb8_keeps_neutral_grays() {
        for v in [0u8, 1, 127, 128, 254, 255] {
            assert_eq!(RGB24(v, v, v).to_bb8(), BB8(v));
        }
        assert_eq!(RGB24(255, 0, 0).to_bb8(), BB8(53));
        assert_eq!(RGB24(0, 255, 0).to_bb8(), BB8(182));
        assert_eq!(RGB24(0, 0, 255).to_bb8(), BB8(18));
    }

    #[test]
    fn mix_is_source_over_and_saturates() {
        // Opaque black over white, transparent over white, 50% black over white.
        assert_eq!(BB8(255).mix(BB8(0), 0), BB8(0));
        assert_eq!(BB8(255).mix(BB8(0), 255), BB8(255));
        assert_eq!(BB8(255).mix(BB8(0), 127), BB8(127));
        // Out-of-range premultiplied input must not wrap.
        assert_eq!(BB8(255).mix(BB8(200), 255), BB8(255));
        assert_eq!(
            RGB24(255, 0, 255).mix(RGB24(10, 255, 10), 255),
            RGB24(255, 255, 255)
        );
        assert_eq!(RGB24(200, 100, 50).mix(RGB24(1, 2, 3), 0), RGB24(1, 2, 3));
    }

    fn frame(bytes: &mut [u8], stride: usize, width: usize, height: usize) -> FrameBytes<'_> {
        FrameBytes {
            bytes,
            stride,
            width,
            height,
        }
    }

    #[test]
    fn copy_rect_gray_respects_stride_and_clips() {
        // 3x2 screen with 4-byte rows (one padding byte per row).
        let mut fb = [0xAAu8; 8];
        let src: Vec<BB8> = (1..=6).map(BB8).collect(); // rows of 3
        frame(&mut fb, 4, 3, 2).copy_rect(1, 0, 10, 10, &src, 3, (0, 0));
        assert_eq!(fb, [0xAA, 2, 3, 0xAA, 0xAA, 5, 6, 0xAA]);
    }

    #[test]
    fn copy_rect_rgb_writes_three_bytes_per_pixel() {
        // 2x1 screen, 6-byte row.
        let mut fb = [0u8; 6];
        let src = [RGB24(1, 2, 3), RGB24(4, 5, 6)];
        frame(&mut fb, 6, 2, 1).copy_rect(1, 0, 1, 1, &src, 2, (0, 0));
        assert_eq!(fb, [0, 0, 0, 4, 5, 6]);
    }

    #[test]
    fn copy_rect_single_pixel_with_origin() {
        let mut fb = [0u8; 4];
        frame(&mut fb, 2, 2, 2).copy_rect(1, 1, 1, 1, &[BB8(9)], 1, (1, 1));
        assert_eq!(fb, [0, 0, 0, 9]);
        // Out of bounds is ignored.
        frame(&mut fb, 2, 2, 2).copy_rect(2, 0, 1, 1, &[BB8(7)], 1, (2, 0));
        assert_eq!(fb, [0, 0, 0, 9]);
    }

    #[test]
    fn copy_rect_stops_at_short_source() {
        let mut fb = [0u8; 4];
        frame(&mut fb, 2, 2, 2).copy_rect(0, 0, 2, 2, &[BB8(1), BB8(2), BB8(3)], 2, (0, 0));
        assert_eq!(fb, [1, 2, 0, 0]);
    }
}
