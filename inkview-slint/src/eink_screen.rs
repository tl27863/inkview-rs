//! The screen operations the backend needs, so the same presenter and damage
//! policy can drive InkView or a desktop simulation.

use core::ffi::c_int;

use inkview::screen::{PanelMode, PixelFormat, Screen, UpdFlags, UpdateKind, Waveform};

/// An e-ink screen: a framebuffer plus asynchronous panel updates.
pub trait EinkScreen {
    /// Pixel layout of the framebuffer.
    type Pixel: PixelFormat + Default;

    /// Physical width in pixels.
    fn width(&self) -> usize;
    /// Physical height in pixels, including any panel rows.
    fn height(&self) -> usize;
    /// Dots per inch; 0 if unknown.
    fn dpi(&self) -> u32;
    /// Firmware UI zoom (informational).
    fn scale(&self) -> f32;

    /// Copies the `w`×`h` rectangle at `(x, y)` into the framebuffer from
    /// `src` (rows of `src_stride` pixels whose first pixel is the framebuffer
    /// pixel `src_origin`).
    #[allow(clippy::too_many_arguments)]
    fn copy_rect(
        &mut self,
        x: usize,
        y: usize,
        w: usize,
        h: usize,
        src: &[Self::Pixel],
        src_stride: usize,
        src_origin: (usize, usize),
    );

    /// Starts a named update of a framebuffer region.
    fn update(&mut self, kind: UpdateKind, x: i32, y: i32, w: u32, h: u32);
    /// Starts an update with an explicit waveform; false if unsupported.
    fn update_ex(
        &mut self,
        waveform: Waveform,
        flags: UpdFlags,
        x: i32,
        y: i32,
        w: u32,
        h: u32,
    ) -> bool;
    /// Whether an update is still in progress on the panel.
    fn is_updating(&mut self) -> bool;
    /// Wakes the platform event loop after an update, if it needs that.
    fn wake_event_loop(&self) -> Option<c_int>;
    /// Per-waveform durations in milliseconds, if known.
    fn waveform_times(&self) -> Option<[u16; 16]>;

    /// Rows at the top reserved for the system panel (0 if none).
    fn panel_height(&self) -> usize {
        0
    }
    /// Redraws the system panel into the framebuffer.
    fn draw_panel(&mut self) {}
    /// One line about the panel for the backend's startup log.
    fn panel_info(&self) -> Option<String> {
        None
    }
}

impl<P: PixelFormat + Default> EinkScreen for Screen<'static, P> {
    type Pixel = P;

    fn width(&self) -> usize {
        Screen::width(self)
    }

    fn height(&self) -> usize {
        Screen::height(self)
    }

    fn dpi(&self) -> u32 {
        Screen::dpi(self)
    }

    fn scale(&self) -> f32 {
        Screen::scale(self)
    }

    fn copy_rect(
        &mut self,
        x: usize,
        y: usize,
        w: usize,
        h: usize,
        src: &[P],
        src_stride: usize,
        src_origin: (usize, usize),
    ) {
        self.copy_rect_from(x, y, w, h, src, src_stride, src_origin);
    }

    fn update(&mut self, kind: UpdateKind, x: i32, y: i32, w: u32, h: u32) {
        Screen::update(self, kind, x, y, w, h);
    }

    fn update_ex(
        &mut self,
        waveform: Waveform,
        flags: UpdFlags,
        x: i32,
        y: i32,
        w: u32,
        h: u32,
    ) -> bool {
        Screen::update_ex(self, waveform, flags, x, y, w, h)
    }

    fn is_updating(&mut self) -> bool {
        Screen::is_updating(self)
    }

    fn wake_event_loop(&self) -> Option<c_int> {
        Screen::wake_event_loop(self)
    }

    fn waveform_times(&self) -> Option<[u16; 16]> {
        Screen::waveform_times(self)
    }

    fn panel_height(&self) -> usize {
        Screen::panel_height(self)
    }

    fn draw_panel(&mut self) {
        Screen::draw_panel(self);
    }

    fn panel_info(&self) -> Option<String> {
        // Hidden is the default; only log when the panel was asked for, and
        // then always, so a panel that reports no usable height is visible.
        (self.panel_mode() == PanelMode::System).then(|| {
            format!(
                "panel mode=system height={} (raw {:?}) fb_offset={:?}",
                Screen::panel_height(self),
                self.panel_height_raw(),
                self.panel_fb_offset()
            )
        })
    }
}
