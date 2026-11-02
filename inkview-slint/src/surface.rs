//! The screen, the system panel rows above the content, and the retained
//! frame of the content area.

use crate::eink_screen::EinkScreen;
use crate::policy::UpdateMode;

pub type Rect = euclid::Rect<i32, euclid::UnknownUnit>;

/// Compact `WxH+X+Y` form for logs.
pub(crate) fn fmt_rect(r: &Rect) -> String {
    format!("{}x{}+{}+{}", r.width(), r.height(), r.origin.x, r.origin.y)
}

pub(crate) struct Surface<S: EinkScreen> {
    pub screen: S,
    pub width: usize,
    /// Full physical height, including the panel rows.
    pub height: usize,
    /// Rows at the top owned by the system panel; content renders below them.
    pub top: usize,
    /// Retained frame of the content area (`width` × `height - top`).
    pub frame: Vec<S::Pixel>,
    wake_logged: bool,
}

impl<S: EinkScreen> Surface<S> {
    pub fn new(screen: S) -> Self {
        let width = screen.width();
        let height = screen.height();
        let top = screen.panel_height();
        Self {
            screen,
            width,
            height,
            top,
            frame: vec![S::Pixel::default(); width * (height - top)],
            wake_logged: false,
        }
    }

    /// Rows of the content area below the panel.
    pub fn content_height(&self) -> usize {
        self.height - self.top
    }

    /// Re-reads the panel height. If it changed, resizes the retained frame;
    /// returns true so the caller resizes its window and renders in full.
    pub fn refresh_panel_height(&mut self) -> bool {
        let top = self.screen.panel_height();
        if top == self.top {
            return false;
        }
        eink_log!("panel height {} -> {top}", self.top);
        self.top = top;
        self.frame = vec![S::Pixel::default(); self.width * self.content_height()];
        true
    }

    pub fn is_updating(&mut self) -> bool {
        self.screen.is_updating()
    }

    /// Issues `mode` for a rectangle in content coordinates.
    pub fn issue(&mut self, mode: UpdateMode, rect: &Rect) {
        mode.issue(
            &mut self.screen,
            rect.origin.x,
            rect.origin.y + self.top as i32,
            rect.width() as u32,
            rect.height() as u32,
        );
    }

    /// Issues `mode` over the whole physical screen, panel rows included.
    pub fn issue_full_screen(&mut self, mode: UpdateMode) {
        mode.issue(
            &mut self.screen,
            0,
            0,
            self.width as u32,
            self.height as u32,
        );
    }

    /// Wakes InkView's main loop after a screen update.
    ///
    /// Updates are requested from the Slint thread while `InkViewMain` runs on
    /// the main thread. On the PB710 an update could stay unprocessed (panel
    /// reported idle after ~100 ms, nothing drawn) until the next touch woke
    /// that loop. See `Screen::wake_event_loop`.
    pub fn wake(&mut self) {
        let rc = self.screen.wake_event_loop();
        if !std::mem::replace(&mut self.wake_logged, true) {
            eink_log!("waking InkView loop after updates: SendEventTo(EVT_CUSTOM) rc={rc:?}");
        }
    }

    /// Copies a damaged rectangle of the retained frame (content
    /// coordinates) into the task framebuffer.
    pub fn copy_damage(&mut self, rect: &Rect) {
        self.screen.copy_rect(
            rect.origin.x.max(0) as usize,
            rect.origin.y.max(0) as usize + self.top,
            rect.width() as usize,
            rect.height() as usize,
            &self.frame,
            self.width,
            (0, self.top),
        );
    }

    /// Copy the retained frame into the task framebuffer.
    ///
    /// InkView can blank or replace the task framebuffer while another task is
    /// in front. Re-blitting the retained frame before a full-screen update
    /// makes a show/resume independent of whatever currently sits in that
    /// buffer. With a system panel, only the rows below it are copied and the
    /// panel is redrawn, so the following full-screen update shows both.
    pub fn blit_all(&mut self) {
        self.screen.copy_rect(
            0,
            self.top,
            self.width,
            self.content_height(),
            &self.frame,
            self.width,
            (0, self.top),
        );
        if self.top > 0 {
            self.screen.draw_panel();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock_screen::{Call, MockScreen};
    use inkview::screen::UpdateKind;

    fn surface_with_panel(top: usize) -> (Surface<MockScreen>, MockScreen) {
        let screen = MockScreen::new(100, 200);
        screen.state.borrow_mut().panel_height = top;
        (Surface::new(screen.clone()), screen)
    }

    #[test]
    fn blit_copies_content_rows_and_redraws_the_panel() {
        let (mut surface, screen) = surface_with_panel(20);
        assert_eq!(surface.frame.len(), 100 * 180);
        surface.blit_all();
        assert_eq!(
            screen.take_calls(),
            [
                Call::CopyRect {
                    x: 0,
                    y: 20,
                    w: 100,
                    h: 180
                },
                Call::DrawPanel
            ]
        );

        let (mut surface, screen) = surface_with_panel(0);
        surface.blit_all();
        assert_eq!(
            screen.take_calls(),
            [Call::CopyRect {
                x: 0,
                y: 0,
                w: 100,
                h: 200
            }]
        );
    }

    #[test]
    fn updates_and_copies_are_shifted_below_the_panel() {
        let (mut surface, screen) = surface_with_panel(20);
        let rect = Rect::new(euclid::Point2D::new(5, 6), euclid::Size2D::new(7, 8));
        surface.copy_damage(&rect);
        surface.issue(UpdateMode::Named(UpdateKind::Partial), &rect);
        surface.issue_full_screen(UpdateMode::Named(UpdateKind::Partial));
        assert_eq!(
            screen.take_calls(),
            [
                Call::CopyRect {
                    x: 5,
                    y: 26,
                    w: 7,
                    h: 8
                },
                Call::Update {
                    kind: UpdateKind::Partial,
                    x: 5,
                    y: 26,
                    w: 7,
                    h: 8
                },
                Call::Update {
                    kind: UpdateKind::Partial,
                    x: 0,
                    y: 0,
                    w: 100,
                    h: 200
                },
            ]
        );
    }

    #[test]
    fn panel_height_change_resizes_the_frame() {
        let (mut surface, screen) = surface_with_panel(20);
        assert!(!surface.refresh_panel_height());
        screen.state.borrow_mut().panel_height = 30;
        assert!(surface.refresh_panel_height());
        assert_eq!(surface.top, 30);
        assert_eq!(surface.content_height(), 170);
        assert_eq!(surface.frame.len(), 100 * 170);
    }
}
