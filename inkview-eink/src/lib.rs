//! The e-ink logic of an InkView UI backend, without a UI toolkit: full
//! presents (screen transitions, show and resume), the damage path (partial,
//! dynamic, finalizer, fallback), the update policy, the retained frame below
//! the system panel, and the wake after each update.
//!
//! A backend owns an [`EinkPresenter`], calls it in the order documented on
//! that type, and renders into its retained frame. `inkview-slint` is one
//! such backend.

use std::{
    sync::OnceLock,
    time::{Duration, Instant},
};

use inkview::Event as InkEvent;

static EPOCH: OnceLock<Instant> = OnceLock::new();

#[doc(hidden)]
pub use log as __log;

/// Milliseconds since the first `eink_log!` line.
#[doc(hidden)]
pub fn since_epoch_ms() -> u128 {
    EPOCH.get_or_init(Instant::now).elapsed().as_millis()
}

/// Backend log line through the `log` facade (target `eink`), so the
/// application chooses where it goes and at which level. The first line
/// starts the `+<ms>` epoch.
#[macro_export]
macro_rules! eink_log {
    ($($arg:tt)*) => {
        $crate::__log::info!(target: "eink", "[eink +{}] {}", $crate::since_epoch_ms(), format_args!($($arg)*))
    };
}

mod clock;
mod damage;
mod input;
#[cfg(test)]
mod mock_screen;
mod policy;
mod present;
mod queue;
mod screen;
mod surface;

pub use input::{PointerKind, content_pointer};
pub use policy::{UPDATES_ENV, UpdateMode, UpdatePolicy};
pub use present::{full_present_requested, request_full_present};
pub use queue::{Disconnected, wait_events};
pub use screen::EinkScreen;
pub use surface::Rect;

use clock::Clock;
use damage::DamagePath;
use present::{PresentReason, Presenter, take_full_present_request};
use surface::{Surface, fmt_rect};

/// Used when the screen reports no usable DPI (see `scale_from_dpi`).
/// The pbanki UI was laid out for 3.0 on a 300 dpi panel.
pub const DEFAULT_SCALE_FACTOR: f32 = 3.0;

/// `dpi / 100`, so a logical pixel has the same physical size on every panel,
/// or `DEFAULT_SCALE_FACTOR` if that is out of the plausible range (some
/// firmware reports 0).
///
/// InkView's screen scale factor is not applied: it carries the firmware's UI
/// zoom (1.16 on a PB710 at 300 dpi), which would enlarge the whole layout.
pub fn scale_from_dpi(dpi: u32) -> f32 {
    let scale = dpi as f32 / 100.0;
    if (1.5..=6.0).contains(&scale) {
        scale
    } else {
        DEFAULT_SCALE_FACTOR
    }
}

/// Presenter settings beyond the screen itself.
#[derive(Clone, Copy, Debug, Default)]
pub struct EinkOptions {
    /// Update mode per class of screen change. `INKVIEW_SLINT_UPDATES`
    /// overrides individual classes on top of this.
    pub policy: UpdatePolicy,
    /// Log every InkView event (`INKVIEW_SLINT_TRACE_EVENTS`).
    pub trace_events: bool,
}

/// What the backend must do after an InkView event.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Lifecycle {
    /// Render again (show, resume, repaint).
    pub redraw: bool,
    /// The content area changed size; resize the window.
    pub resized: bool,
}

/// The e-ink logic of a backend: full presents, the damage path, the update
/// policy and the retained frame below the system panel.
///
/// One iteration of the backend's loop calls, in this order (each step was
/// a device regression when it moved, see pbAnki's `notes/eink-refresh.md`):
///
/// 1. The backend's own timers and animations.
/// 2. [`sample_panel`](Self::sample_panel).
/// 3. Wait for events: no wait if [`full_present_requested`]
///    is set, else at most the backend's timers and [`deadline`](Self::deadline) (max 1 s).
/// 4. Drain the queue, so Show and Foreground become one present.
/// 5. [`before_dispatch`](Self::before_dispatch) (the finalizer).
/// 6. For each InkView event: [`on_event`](Self::on_event), then the input
///    for the UI (pointer positions through `content_pointer`).
/// 7. The backend's timers again, so handlers that input queued render now.
/// 8. [`after_dispatch`](Self::after_dispatch) (application present request,
///    verification). It must come after step 7: the request is read after
///    the UI's change handlers ran.
/// 9. If the UI has to redraw: render into [`frame_mut`](Self::frame_mut)
///    (in full if [`full_render_needed`](Self::full_render_needed)), then
///    [`submit_frame`](Self::submit_frame).
/// 10. [`after_render`](Self::after_render) (issues a due full present).
pub struct EinkPresenter<S: EinkScreen> {
    clock: Clock,
    surface: Surface<S>,
    policy: UpdatePolicy,
    presenter: Presenter,
    damage: DamagePath,
    trace_events: bool,
    /// The next render must repaint the whole frame.
    force_full_render: bool,
    /// A request in this iteration started a new present.
    new_present: bool,
}

impl<S: EinkScreen> EinkPresenter<S> {
    /// Logs the update policy (with `INKVIEW_SLINT_UPDATES` applied), the
    /// waveform times and the panel. Call [`start`](Self::start) when the
    /// event loop starts.
    pub fn new(screen: S, options: EinkOptions) -> Self {
        Self::with_clock(screen, options, Clock::default())
    }

    pub(crate) fn with_clock(screen: S, options: EinkOptions, clock: Clock) -> Self {
        let mut policy = options.policy;
        if let Ok(spec) = std::env::var(UPDATES_ENV) {
            for ignored in policy.apply_overrides(&spec) {
                eink_log!("{UPDATES_ENV}: ignored {ignored}");
            }
        }
        eink_log!("update policy: {policy}");
        match screen.waveform_times() {
            Some(times) => eink_log!("waveform times (ms, by waveform id): {times:?}"),
            None => eink_log!("waveform times unavailable"),
        }
        if let Some(info) = screen.panel_info() {
            eink_log!("{info}");
        }

        Self {
            surface: Surface::new(screen),
            policy,
            presenter: Presenter::idle(clock.clone()),
            damage: DamagePath::new(clock.clone()),
            clock,
            trace_events: options.trace_events,
            force_full_render: false,
            new_present: false,
        }
    }

    /// Requests the launch present. Call once, when the event loop starts.
    pub fn start(&mut self) {
        self.presenter = Presenter::new(self.clock.clone());
        self.damage = DamagePath::new(self.clock.clone());
        self.force_full_render = false;
        self.new_present = false;
    }

    /// Physical width.
    pub fn width(&self) -> usize {
        self.surface.width
    }

    /// Size of the content area below the system panel.
    pub fn content_size(&self) -> (usize, usize) {
        (self.surface.width, self.surface.content_height())
    }

    /// Rows at the top owned by the system panel.
    pub fn top(&self) -> usize {
        self.surface.top
    }

    /// Step 2: samples the panel (logs busy→idle).
    pub fn sample_panel(&mut self) {
        self.presenter.sample_panel(&mut self.surface.screen);
    }

    /// Step 3: time until the presenter needs the loop again (finalizer,
    /// pending present or verification, panel sampling).
    pub fn deadline(&self) -> Option<Duration> {
        let now = self.clock.now();
        [
            self.damage.deadline(now),
            self.presenter
                .next_deadline()
                .map(|at| at.saturating_duration_since(now)),
            self.presenter.panel_poll_interval(),
        ]
        .into_iter()
        .flatten()
        .min()
    }

    /// Step 5: the finalizer of the damage path.
    pub fn before_dispatch(&mut self) {
        self.damage.finalize(&mut self.surface, &self.policy);
    }

    /// Step 6, for each InkView event before it goes to the UI.
    pub fn on_event(&mut self, evt: &InkEvent) -> Lifecycle {
        if self.trace_events {
            eink_log!("evt: {evt:?}");
        }
        match evt {
            // Taps and keys only, so stalls can be matched to input;
            // PointerMove is too frequent to log.
            InkEvent::PointerDown { x, y } => eink_log!("pointer down {x},{y}"),
            InkEvent::PointerUp { x, y } => eink_log!("pointer up {x},{y}"),
            InkEvent::KeyDown { key } => eink_log!("key down {key:?}"),
            _ => {}
        }
        let mut lifecycle = Lifecycle::default();
        match evt {
            InkEvent::Show | InkEvent::Repaint | InkEvent::Foreground { .. } => {
                self.presenter.visible = true;
                if self.surface.refresh_panel_height() {
                    self.force_full_render = true;
                    lifecycle.resized = true;
                }
                let reason = match evt {
                    InkEvent::Show => PresentReason::Show,
                    InkEvent::Repaint => PresentReason::Repaint,
                    _ => PresentReason::Foreground,
                };
                self.new_present |= self.presenter.request(reason);
                lifecycle.redraw = true;
            }
            InkEvent::Background { .. } | InkEvent::Hide => {
                // Do not present into a task that is no longer visible.
                self.presenter.hide();
                self.force_full_render = false;
            }
            _ => {}
        }
        lifecycle
    }

    /// Step 8, after the UI handled the events and its timers. Consumes the
    /// application's present request, so the present contains the screen
    /// that requested it. Returns true if the UI must redraw.
    pub fn after_dispatch(&mut self) -> bool {
        let mut redraw = false;
        if take_full_present_request() && self.presenter.visible {
            // Consume after timers/event dispatch but before rendering,
            // so the full present contains the screen that requested it
            // and the normal damage path does not race it.
            self.new_present |= self.presenter.request(PresentReason::App);
            redraw = true;
        }

        self.presenter.poll_verification(self.clock.now());

        if std::mem::take(&mut self.new_present) {
            // The full present below covers the whole frame; drop any
            // pending dynamic presentation.
            self.damage.drop_waiting();
        }

        if self.presenter.is_pending() && !self.presenter.have_frame {
            // No complete frame yet. Force one full render so the re-blit
            // below has valid pixels.
            self.force_full_render = true;
            redraw = true;
        }
        redraw
    }

    /// Step 9: the next render must repaint the whole frame.
    pub fn full_render_needed(&self) -> bool {
        self.force_full_render
    }

    /// Step 9: the retained frame of the content area, `width()` pixels per
    /// row. Render into it, then call `submit_frame`.
    pub fn frame_mut(&mut self) -> &mut [S::Pixel] {
        &mut self.surface.frame
    }

    /// Step 9: the render is done. `damage` is the changed area in content
    /// coordinates; `full` is true if the whole frame was repainted.
    pub fn submit_frame(&mut self, damage: Rect, render: Duration, full: bool) {
        if full {
            self.force_full_render = false;
        }

        let full_damage = damage.width() >= self.surface.width as i32
            && damage.height() >= self.surface.content_height() as i32;
        if !self.presenter.have_frame && (full || full_damage) {
            self.presenter.have_frame = true;
            eink_log!("first full frame rendered in {}ms", render.as_millis());
        } else if render > Duration::from_millis(500) {
            eink_log!("slow render: {}ms damage={damage:?}", render.as_millis());
        }

        if self.presenter.is_pending() {
            // The pending full present re-blits and presents the whole
            // retained frame; damage only updates the retained buffer.
            eink_log!("damage {} held for pending present", fmt_rect(&damage));
            return;
        }

        if damage.is_empty() {
            // Nothing changed on screen (e.g. a tap on a control
            // without pressed styles); issue no zero-area update.
            return;
        }

        self.damage
            .submit(&mut self.surface, &self.policy, damage, render);
    }

    /// Step 10: issues a pending full present once it is due.
    pub fn after_render(&mut self) {
        if self
            .presenter
            .issue_due(&mut self.surface, &self.policy, self.force_full_render)
        {
            self.damage.presented();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock_screen::{Call, MockScreen};
    use crate::present::request_full_present;
    use crate::present::tests::LATCH;
    use inkview::screen::UpdateKind;

    const MS: fn(u64) -> Duration = Duration::from_millis;
    const W: usize = 400;
    const H: usize = 400;
    const TOP: usize = 20;

    fn setup() -> (Clock, MockScreen, EinkPresenter<MockScreen>) {
        let clock = Clock::manual();
        let screen = MockScreen::new(W, H).with_update_time(&clock, MS(400));
        screen.state.borrow_mut().panel_height = TOP;
        let mut eink =
            EinkPresenter::with_clock(screen.clone(), EinkOptions::default(), clock.clone());
        eink.start();
        (clock, screen, eink)
    }

    fn rect(x: i32, y: i32, w: i32, h: i32) -> Rect {
        Rect::new(euclid::Point2D::new(x, y), euclid::Size2D::new(w, h))
    }

    fn whole_content() -> Rect {
        rect(0, 0, W as i32, (H - TOP) as i32)
    }

    fn present() -> Vec<Call> {
        vec![
            Call::CopyRect {
                x: 0,
                y: TOP,
                w: W,
                h: H - TOP,
            },
            Call::DrawPanel,
            Call::Update {
                kind: UpdateKind::Partial,
                x: 0,
                y: 0,
                w: W as u32,
                h: H as u32,
            },
            Call::Wake,
        ]
    }

    /// One loop iteration without events. `damage` is what the UI rendered,
    /// if it redraws.
    fn iterate(eink: &mut EinkPresenter<MockScreen>, events: &[InkEvent], damage: Option<Rect>) {
        eink.sample_panel();
        eink.before_dispatch();
        let mut redraw = false;
        for evt in events {
            redraw |= eink.on_event(evt).redraw;
        }
        redraw |= eink.after_dispatch();
        if let Some(damage) = damage {
            let full = eink.full_render_needed();
            eink.submit_frame(damage, MS(10), full);
        } else {
            assert!(!redraw, "the presenter asked for a redraw");
        }
        eink.after_render();
    }

    #[test]
    fn launch_present_verify_then_content_updates() {
        let _latch = LATCH.lock().unwrap_or_else(|e| e.into_inner());
        let (clock, screen, mut eink) = setup();

        // Launch: Show and Foreground in one drain coalesce into the launch
        // present; the first frame is rendered in full and held for it.
        iterate(
            &mut eink,
            &[InkEvent::Show, InkEvent::Foreground { pid: 0 }],
            Some(whole_content()),
        );
        assert_eq!(screen.take_calls(), present());
        assert_eq!(eink.deadline(), Some(crate::present::PANEL_POLL));

        // Verification 300 ms after the issue; the panel is busy for 400 ms,
        // so it waits until the panel is idle.
        clock.advance(MS(300));
        iterate(&mut eink, &[], None);
        assert!(screen.take_calls().is_empty());
        clock.advance(MS(100));
        iterate(&mut eink, &[], None);
        assert_eq!(screen.take_calls(), present());
        assert_eq!(eink.deadline(), Some(crate::present::PANEL_POLL));

        // Idle panel: a partial update of the damage.
        clock.advance(MS(500));
        let answer = rect(10, 10, 50, 50);
        iterate(&mut eink, &[], Some(answer));
        assert_eq!(
            screen.take_calls(),
            [
                Call::CopyRect {
                    x: 10,
                    y: 30,
                    w: 50,
                    h: 50
                },
                Call::Update {
                    kind: UpdateKind::Partial,
                    x: 10,
                    y: 30,
                    w: 50,
                    h: 50
                },
                Call::Wake,
            ]
        );

        // Busy panel, small damage: dynamic, then the finalizer.
        clock.advance(MS(30));
        let tap = rect(20, 20, 10, 10);
        iterate(&mut eink, &[], Some(tap));
        assert_eq!(
            screen.take_calls(),
            [
                Call::CopyRect {
                    x: 20,
                    y: 40,
                    w: 10,
                    h: 10
                },
                Call::Update {
                    kind: UpdateKind::Dynamic,
                    x: 20,
                    y: 40,
                    w: 10,
                    h: 10
                },
                Call::Wake,
            ]
        );
        clock.advance(MS(201));
        iterate(&mut eink, &[], None);
        assert!(screen.take_calls().is_empty());
        clock.advance(MS(200));
        iterate(&mut eink, &[], None);
        assert_eq!(
            screen.take_calls(),
            [
                Call::Update {
                    kind: UpdateKind::Partial,
                    x: 20,
                    y: 40,
                    w: 10,
                    h: 10
                },
                Call::Wake,
            ]
        );
    }

    #[test]
    fn app_present_drops_waiting_damage_and_presents_the_new_frame() {
        let _latch = LATCH.lock().unwrap_or_else(|e| e.into_inner());
        let (clock, screen, mut eink) = setup();
        iterate(&mut eink, &[], Some(whole_content()));
        clock.advance(MS(300));
        iterate(&mut eink, &[], None);
        clock.advance(MS(100));
        iterate(&mut eink, &[], None);
        screen.take_calls();

        // Busy, large damage: waits for the finalizer.
        clock.advance(MS(30));
        screen.set_busy(true);
        iterate(&mut eink, &[], Some(whole_content()));
        assert_eq!(
            screen.take_calls(),
            [Call::CopyRect {
                x: 0,
                y: TOP,
                w: W,
                h: H - TOP
            }]
        );

        // The app requests a present: the damage of this render is held and
        // the present goes out once the panel is idle; the finalizer has
        // nothing left.
        request_full_present();
        let screen_change = rect(0, 0, 80, 80);
        iterate(&mut eink, &[], Some(screen_change));
        assert!(screen.take_calls().is_empty());
        // Idle once the verification update (400 ms) is done.
        screen.set_busy(false);
        clock.advance(MS(400));
        iterate(&mut eink, &[], None);
        assert_eq!(screen.take_calls(), present());
        clock.advance(MS(250));
        iterate(&mut eink, &[], None);
        assert!(screen.take_calls().is_empty());
    }

    #[test]
    fn hidden_task_gets_no_present() {
        let _latch = LATCH.lock().unwrap_or_else(|e| e.into_inner());
        let (_clock, screen, mut eink) = setup();
        iterate(&mut eink, &[InkEvent::Background { pid: 0 }], None);
        request_full_present();
        iterate(&mut eink, &[], None);
        assert!(screen.take_calls().is_empty());
        assert!(!crate::present::full_present_requested());
    }

    #[test]
    fn empty_damage_sends_no_update() {
        let _latch = LATCH.lock().unwrap_or_else(|e| e.into_inner());
        let (clock, screen, mut eink) = setup();
        iterate(&mut eink, &[], Some(whole_content()));
        clock.advance(MS(1000));
        iterate(&mut eink, &[], None);
        clock.advance(MS(1000));
        iterate(&mut eink, &[], None);
        screen.take_calls();

        clock.advance(MS(1000));
        iterate(&mut eink, &[], Some(rect(0, 0, 0, 0)));
        assert!(screen.take_calls().is_empty());
    }

    #[test]
    fn panel_height_change_on_show_resizes_and_renders_in_full() {
        let _latch = LATCH.lock().unwrap_or_else(|e| e.into_inner());
        let (_clock, screen, mut eink) = setup();
        screen.state.borrow_mut().panel_height = 30;
        let lifecycle = eink.on_event(&InkEvent::Show);
        assert_eq!(
            lifecycle,
            Lifecycle {
                redraw: true,
                resized: true
            }
        );
        assert_eq!(eink.content_size(), (W, H - 30));
        assert_eq!(eink.frame_mut().len(), W * (H - 30));
        assert!(eink.full_render_needed());
        eink.submit_frame(rect(0, 0, W as i32, (H - 30) as i32), MS(10), true);
        assert!(!eink.full_render_needed());
    }

    #[test]
    fn scale_follows_dpi_with_fallback() {
        assert_eq!(scale_from_dpi(300), 3.0);
        assert_eq!(scale_from_dpi(212), 2.12);
        assert_eq!(scale_from_dpi(0), DEFAULT_SCALE_FACTOR);
        assert_eq!(scale_from_dpi(1000), DEFAULT_SCALE_FACTOR);
    }
}
