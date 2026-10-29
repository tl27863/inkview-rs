use crate::bindings::{APPLICATION_ATTRIBUTE_APPLICATION_READER, EVT_CUSTOM};
use crate::error;
use crate::{bindings::icanvas_s, bindings::Inkview};
use core::ffi::c_int;
use core::fmt::Display;
use num_traits::{FromPrimitive, ToPrimitive};

pub struct Screen<'a> {
    iv: &'a Inkview,

    #[allow(unused)]
    fb: &'a mut icanvas_s,
    width: usize,
    height: usize,
    stride: usize,
    buf: *mut u8,
    task: c_int,
}

impl<'a> Screen<'a> {
    pub fn new(iv: &'a Inkview) -> Self {
        unsafe {
            iv.SetCurrentApplicationAttribute(APPLICATION_ATTRIBUTE_APPLICATION_READER, 1);
        }
        let task = unsafe { iv.GetCurrentTask() };
        let fb = unsafe { iv.GetTaskFramebuffer(task).as_mut() }
            .expect("Failed to get current task framebuffer while creating new screen.");

        let width = fb.width as usize;
        let height = fb.height as usize;
        let stride = fb.scanline as usize;
        let buf = fb.addr;

        Self {
            iv,
            fb,
            width,
            height,
            stride,
            buf,
            task,
        }
    }

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

    pub fn clear(&mut self) {
        unsafe {
            self.iv.ClearScreen();
        }
    }

    #[inline(always)]
    pub fn draw(&mut self, x: usize, y: usize, c: u8) {
        if !(0..self.width).contains(&x) || !(0..self.height).contains(&y) {
            return;
        }
        let i = self.stride * y + x;

        unsafe {
            self.buf.add(i).write(c);
        }
    }

    /// Copy a rectangle of 8-bit gray pixels into the framebuffer, one row at a
    /// time.
    ///
    /// `src` holds rows of `src_stride` pixels; the rectangle is read from the
    /// same `(x, y)` offset in `src` as it is written to in the framebuffer.
    /// The rectangle is clipped to the screen and to `src`.
    pub fn copy_gray_rect(
        &mut self,
        x: usize,
        y: usize,
        w: usize,
        h: usize,
        src: &[u8],
        src_stride: usize,
    ) {
        let x_end = (x + w).min(self.width).min(src_stride);
        let y_end = (y + h).min(self.height);
        if x >= x_end {
            return;
        }
        let len = x_end - x;

        for row in y..y_end {
            let src_start = row * src_stride + x;
            let Some(src_row) = src.get(src_start..src_start + len) else {
                return;
            };
            // SAFETY: `row < height` and `x + len <= width <= stride`, so the
            // destination row lies within the task framebuffer.
            let dst = unsafe {
                core::slice::from_raw_parts_mut(self.buf.add(self.stride * row + x), len)
            };
            dst.copy_from_slice(src_row);
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

    /// Block until the current panel update has finished.
    ///
    /// Only use this for infrequent transitions (show/resume or a large
    /// loading/card change); it can block for the duration of an e-ink update.
    pub fn wait_for_update_complete(&self) {
        unsafe {
            self.iv.WaitForUpdateComplete();
        }
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
