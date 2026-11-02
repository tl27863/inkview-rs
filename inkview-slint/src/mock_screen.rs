//! Test-only `EinkScreen` that records the calls the backend makes.

use core::ffi::c_int;
use std::{cell::RefCell, rc::Rc};

use inkview::screen::{BB8, UpdFlags, UpdateKind, Waveform};

use crate::eink_screen::EinkScreen;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Call {
    CopyRect {
        x: usize,
        y: usize,
        w: usize,
        h: usize,
    },
    Update {
        kind: UpdateKind,
        x: i32,
        y: i32,
        w: u32,
        h: u32,
    },
    UpdateEx {
        waveform: Waveform,
        x: i32,
        y: i32,
        w: u32,
        h: u32,
    },
    DrawPanel,
    Wake,
}

#[derive(Default)]
pub(crate) struct MockState {
    pub calls: Vec<Call>,
    /// What `is_updating` reports.
    pub busy: bool,
    pub panel_height: usize,
}

/// Shares its state with the test through `state`, so the test can script the
/// panel and read the calls while the backend owns the screen.
#[derive(Clone)]
pub(crate) struct MockScreen {
    pub state: Rc<RefCell<MockState>>,
    pub width: usize,
    pub height: usize,
}

impl MockScreen {
    pub fn new(width: usize, height: usize) -> Self {
        Self {
            state: Default::default(),
            width,
            height,
        }
    }

    pub fn set_busy(&self, busy: bool) {
        self.state.borrow_mut().busy = busy;
    }

    /// Returns and clears the recorded calls.
    pub fn take_calls(&self) -> Vec<Call> {
        std::mem::take(&mut self.state.borrow_mut().calls)
    }
}

impl EinkScreen for MockScreen {
    type Pixel = BB8;

    fn width(&self) -> usize {
        self.width
    }

    fn height(&self) -> usize {
        self.height
    }

    fn dpi(&self) -> u32 {
        300
    }

    fn scale(&self) -> f32 {
        1.0
    }

    fn copy_rect(
        &mut self,
        x: usize,
        y: usize,
        w: usize,
        h: usize,
        _src: &[BB8],
        _src_stride: usize,
        _src_origin: (usize, usize),
    ) {
        self.state
            .borrow_mut()
            .calls
            .push(Call::CopyRect { x, y, w, h });
    }

    fn update(&mut self, kind: UpdateKind, x: i32, y: i32, w: u32, h: u32) {
        self.state
            .borrow_mut()
            .calls
            .push(Call::Update { kind, x, y, w, h });
    }

    fn update_ex(
        &mut self,
        waveform: Waveform,
        _flags: UpdFlags,
        x: i32,
        y: i32,
        w: u32,
        h: u32,
    ) -> bool {
        self.state.borrow_mut().calls.push(Call::UpdateEx {
            waveform,
            x,
            y,
            w,
            h,
        });
        true
    }

    fn is_updating(&mut self) -> bool {
        self.state.borrow().busy
    }

    fn wake_event_loop(&self) -> Option<c_int> {
        self.state.borrow_mut().calls.push(Call::Wake);
        Some(1)
    }

    fn waveform_times(&self) -> Option<[u16; 16]> {
        None
    }

    fn panel_height(&self) -> usize {
        self.state.borrow().panel_height
    }

    fn draw_panel(&mut self) {
        self.state.borrow_mut().calls.push(Call::DrawPanel);
    }
}
