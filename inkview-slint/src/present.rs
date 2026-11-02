//! Full presents: screen transitions and task show/resume.

use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

use crate::clock::Clock;
use crate::eink_screen::EinkScreen;
use crate::policy::UpdatePolicy;
use crate::surface::Surface;

/// Delay after an issued full present before verifying it with a second one.
/// The first update can be discarded while InkView is still settling;
/// verification does not delay the first frame.
pub(crate) const VERIFY_DELAY: Duration = Duration::from_millis(300);

/// Longest a full present waits for the panel to report idle. After this the
/// non-flashing update is issued anyway instead of falling back to a flashing
/// `FullUpdate`.
pub(crate) const PRESENT_WAIT_BUDGET: Duration = Duration::from_millis(1200);

/// Poll interval while a full present waits for the panel.
pub(crate) const PRESENT_RETRY: Duration = Duration::from_millis(20);

/// While the panel reports busy, wake at this interval to sample it so the
/// busy→idle log line carries an accurate timestamp.
pub(crate) const PANEL_POLL: Duration = Duration::from_millis(50);
/// Slower sampling once the panel has been busy for longer than this.
const PANEL_POLL_SLOW_AFTER: Duration = Duration::from_secs(10);
const PANEL_POLL_SLOW: Duration = Duration::from_millis(500);

static FULL_PRESENT_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Requests a non-flashing, full-frame `PartialUpdate` after the current Slint
/// damage has been rendered. This does not invoke InkView's flashing
/// `FullUpdate` and is intended only for genuine screen transitions.
///
/// Calls are coalesced. The hook used by pbanki calls this from the UI thread;
/// the flag itself does not wake an event loop blocked in another thread.
pub fn request_full_present() {
    FULL_PRESENT_REQUESTED.store(true, Ordering::Release);
}

/// Whether the application requested a full present that is not consumed yet.
pub fn full_present_requested() -> bool {
    FULL_PRESENT_REQUESTED.load(Ordering::Acquire)
}

/// Consumes the application's request.
pub(crate) fn take_full_present_request() -> bool {
    FULL_PRESENT_REQUESTED.swap(false, Ordering::AcqRel)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PresentReason {
    Launch,
    Show,
    Foreground,
    Repaint,
    App,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PresentKind {
    /// A requested present; followed by one verification.
    Primary,
    /// The verification of an earlier present; not verified again.
    Verify,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum PresentState {
    Idle,
    Pending {
        kind: PresentKind,
        reason: PresentReason,
        since: Instant,
        attempts: u32,
        /// When the panel was first found busy for this present.
        waiting_since: Option<Instant>,
        next_attempt: Instant,
    },
    Verifying {
        due: Instant,
    },
}

/// Owns the invariant "a requested full present is eventually issued and then
/// verified once".
///
/// Requests from InkView events and from the application coalesce: a request
/// while pending only updates the reason, a request while verifying starts a
/// new pending present. A present waits for the panel to be idle for at most
/// `PRESENT_WAIT_BUDGET`, then issues a non-flashing full-screen
/// `PartialUpdate` regardless.
pub(crate) struct Presenter {
    clock: Clock,
    pub state: PresentState,
    pub visible: bool,
    /// The retained buffer holds a complete frame.
    pub have_frame: bool,
    /// Last sampled `IsUpdateInProcess()` and when it turned busy.
    panel_busy_since: Option<Instant>,
}

impl Presenter {
    pub fn new(clock: Clock) -> Self {
        let now = clock.now();
        eink_log!("request reason=Launch");
        Self {
            clock,
            state: PresentState::Pending {
                kind: PresentKind::Primary,
                reason: PresentReason::Launch,
                since: now,
                attempts: 0,
                waiting_since: None,
                next_attempt: now,
            },
            visible: true,
            have_frame: false,
            panel_busy_since: None,
        }
    }

    /// No present yet; `new` starts the launch present.
    pub fn idle(clock: Clock) -> Self {
        Self {
            clock,
            state: PresentState::Idle,
            visible: true,
            have_frame: false,
            panel_busy_since: None,
        }
    }

    pub fn is_pending(&self) -> bool {
        matches!(self.state, PresentState::Pending { .. })
    }

    /// Returns true if this started a new pending present (rather than
    /// coalescing into one), so callers can drop competing damage updates.
    pub fn request(&mut self, new_reason: PresentReason) -> bool {
        let now = self.clock.now();
        match &mut self.state {
            PresentState::Pending { kind, reason, .. } => {
                eink_log!(
                    "request reason={new_reason:?} (coalesced into pending {reason:?} {kind:?})"
                );
                *reason = new_reason;
                *kind = PresentKind::Primary;
                false
            }
            PresentState::Idle | PresentState::Verifying { .. } => {
                eink_log!("request reason={new_reason:?}");
                self.state = PresentState::Pending {
                    kind: PresentKind::Primary,
                    reason: new_reason,
                    since: now,
                    attempts: 0,
                    waiting_since: None,
                    next_attempt: now,
                };
                true
            }
        }
    }

    pub fn hide(&mut self) {
        if !matches!(self.state, PresentState::Idle) {
            eink_log!("hidden; dropping {:?}", self.state_name());
        }
        self.visible = false;
        self.state = PresentState::Idle;
        FULL_PRESENT_REQUESTED.store(false, Ordering::Release);
    }

    fn state_name(&self) -> &'static str {
        match self.state {
            PresentState::Idle => "idle",
            PresentState::Pending { .. } => "pending present",
            PresentState::Verifying { .. } => "verification",
        }
    }

    /// Promotes a due verification to a pending present.
    pub fn poll_verification(&mut self, now: Instant) {
        if let PresentState::Verifying { due } = self.state
            && now >= due
        {
            self.state = PresentState::Pending {
                kind: PresentKind::Verify,
                reason: PresentReason::App,
                since: now,
                attempts: 0,
                waiting_since: None,
                next_attempt: now,
            };
        }
    }

    pub fn next_deadline(&self) -> Option<Instant> {
        match self.state {
            PresentState::Idle => None,
            PresentState::Pending { next_attempt, .. } => Some(next_attempt),
            PresentState::Verifying { due } => Some(due),
        }
    }

    /// Samples the panel and logs busy→idle transitions.
    pub fn sample_panel<S: EinkScreen>(&mut self, screen: &mut S) -> bool {
        let busy = screen.is_updating();
        match (busy, self.panel_busy_since) {
            (true, None) => self.panel_busy_since = Some(self.clock.now()),
            (false, Some(since)) => {
                eink_log!(
                    "panel busy→idle after {}ms",
                    self.clock
                        .now()
                        .saturating_duration_since(since)
                        .as_millis()
                );
                self.panel_busy_since = None;
            }
            _ => {}
        }
        busy
    }

    pub fn panel_poll_interval(&self) -> Option<Duration> {
        self.panel_busy_since.map(|since| {
            if self.clock.now().saturating_duration_since(since) > PANEL_POLL_SLOW_AFTER {
                PANEL_POLL_SLOW
            } else {
                PANEL_POLL
            }
        })
    }

    /// Issues the pending present once it is due, the frame is complete and
    /// no full render is outstanding. While the panel is busy it retries
    /// every 20 ms, for at most `PRESENT_WAIT_BUDGET`. Returns true if it
    /// issued the present.
    pub fn issue_due<S: EinkScreen>(
        &mut self,
        surface: &mut Surface<S>,
        policy: &UpdatePolicy,
        full_render_outstanding: bool,
    ) -> bool {
        let PresentState::Pending {
            kind,
            reason,
            since,
            attempts,
            waiting_since,
            next_attempt,
        } = &mut self.state
        else {
            return false;
        };
        let now = self.clock.now();
        if now < *next_attempt || full_render_outstanding || !self.have_frame {
            return false;
        }

        let (kind, reason, since) = (*kind, *reason, *since);
        let busy = surface.is_updating();
        let waited = waiting_since.map(|at| now - at).unwrap_or_default();

        if busy && waited < PRESENT_WAIT_BUDGET {
            if waiting_since.is_none() {
                *waiting_since = Some(now);
            }
            *attempts += 1;
            *next_attempt = now + PRESENT_RETRY;
            return false;
        }

        let attempts = *attempts;
        let blit_started = self.clock.now();
        surface.blit_all();
        let blit_elapsed = self.clock.now() - blit_started;
        surface.issue_full_screen(policy.present);
        surface.wake();

        match kind {
            PresentKind::Primary => {
                eink_log!(
                    "issued reason={reason:?} after={}ms waited={}ms \
                     attempts={attempts} busy={busy} blit={}ms",
                    (now - since).as_millis(),
                    waited.as_millis(),
                    blit_elapsed.as_millis()
                );
                self.state = PresentState::Verifying {
                    due: self.clock.now() + VERIFY_DELAY,
                };
            }
            PresentKind::Verify => {
                eink_log!(
                    "verify issued waited={}ms attempts={attempts} busy={busy}",
                    waited.as_millis()
                );
                self.state = PresentState::Idle;
            }
        }
        // The issued update makes the panel busy again.
        self.sample_panel(&mut surface.screen);
        true
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::mock_screen::{Call, MockScreen};
    use inkview::screen::UpdateKind;

    /// Serializes tests that use the process-wide full-present latch.
    pub(crate) static LATCH: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn pending(presenter: &Presenter) -> Option<(PresentKind, PresentReason)> {
        match presenter.state {
            PresentState::Pending { kind, reason, .. } => Some((kind, reason)),
            _ => None,
        }
    }

    #[test]
    fn presenter_starts_with_a_launch_present() {
        let presenter = Presenter::new(Clock::manual());
        assert_eq!(
            pending(&presenter),
            Some((PresentKind::Primary, PresentReason::Launch))
        );
        assert!(presenter.visible);
        assert!(!presenter.have_frame);
    }

    #[test]
    fn requests_coalesce_while_pending() {
        let mut presenter = Presenter::new(Clock::manual());
        assert!(!presenter.request(PresentReason::Show));
        assert!(!presenter.request(PresentReason::Foreground));
        assert_eq!(
            pending(&presenter),
            Some((PresentKind::Primary, PresentReason::Foreground))
        );
    }

    #[test]
    fn request_during_verification_starts_a_new_present() {
        let clock = Clock::manual();
        let mut presenter = Presenter::new(clock.clone());
        presenter.state = PresentState::Verifying {
            due: clock.now() + VERIFY_DELAY,
        };
        assert!(presenter.request(PresentReason::App));
        assert_eq!(
            pending(&presenter),
            Some((PresentKind::Primary, PresentReason::App))
        );

        presenter.state = PresentState::Idle;
        assert!(presenter.request(PresentReason::Repaint));
    }

    #[test]
    fn request_upgrades_a_pending_verification_to_primary() {
        let clock = Clock::manual();
        let mut presenter = Presenter::new(clock.clone());
        presenter.state = PresentState::Verifying { due: clock.now() };
        presenter.poll_verification(clock.now());
        assert_eq!(
            pending(&presenter),
            Some((PresentKind::Verify, PresentReason::App))
        );
        assert!(!presenter.request(PresentReason::Show));
        assert_eq!(
            pending(&presenter),
            Some((PresentKind::Primary, PresentReason::Show))
        );
    }

    #[test]
    fn verification_becomes_pending_when_due() {
        let clock = Clock::manual();
        let mut presenter = Presenter::new(clock.clone());
        let due = clock.now() + VERIFY_DELAY;
        presenter.state = PresentState::Verifying { due };
        assert_eq!(presenter.next_deadline(), Some(due));

        presenter.poll_verification(due - Duration::from_millis(1));
        assert!(!presenter.is_pending());
        presenter.poll_verification(due);
        assert_eq!(
            pending(&presenter),
            Some((PresentKind::Verify, PresentReason::App))
        );
    }

    #[test]
    fn hide_drops_the_present_and_the_latch() {
        let _latch = LATCH.lock().unwrap_or_else(|e| e.into_inner());
        let mut presenter = Presenter::new(Clock::manual());
        request_full_present();
        presenter.hide();
        assert!(!presenter.visible);
        assert!(matches!(presenter.state, PresentState::Idle));
        assert_eq!(presenter.next_deadline(), None);
        assert!(!full_present_requested());
    }

    #[test]
    fn panel_sampling_tracks_busy_time() {
        let clock = Clock::manual();
        let mut screen = MockScreen::new(100, 200);
        let mut presenter = Presenter::new(clock.clone());
        assert_eq!(presenter.panel_poll_interval(), None);

        screen.set_busy(true);
        assert!(presenter.sample_panel(&mut screen));
        assert_eq!(presenter.panel_poll_interval(), Some(PANEL_POLL));
        clock.advance(PANEL_POLL_SLOW_AFTER + Duration::from_millis(1));
        assert_eq!(presenter.panel_poll_interval(), Some(PANEL_POLL_SLOW));

        screen.set_busy(false);
        assert!(!presenter.sample_panel(&mut screen));
        assert_eq!(presenter.panel_poll_interval(), None);
    }

    fn present_calls() -> Vec<Call> {
        vec![
            Call::CopyRect {
                x: 0,
                y: 20,
                w: 100,
                h: 180,
            },
            Call::DrawPanel,
            Call::Update {
                kind: UpdateKind::Partial,
                x: 0,
                y: 0,
                w: 100,
                h: 200,
            },
            Call::Wake,
        ]
    }

    fn setup() -> (Clock, MockScreen, Surface<MockScreen>, Presenter) {
        let clock = Clock::manual();
        let screen = MockScreen::new(100, 200);
        screen.state.borrow_mut().panel_height = 20;
        let surface = Surface::new(screen.clone());
        let presenter = Presenter::new(clock.clone());
        (clock, screen, surface, presenter)
    }

    #[test]
    fn present_waits_for_a_complete_frame() {
        let (_clock, screen, mut surface, mut presenter) = setup();
        let policy = UpdatePolicy::default();
        assert!(!presenter.issue_due(&mut surface, &policy, false));
        presenter.have_frame = true;
        assert!(!presenter.issue_due(&mut surface, &policy, true));
        assert!(screen.take_calls().is_empty());
        assert!(presenter.issue_due(&mut surface, &policy, false));
        assert_eq!(screen.take_calls(), present_calls());
    }

    #[test]
    fn present_is_verified_once_300ms_after_issue() {
        let (clock, screen, mut surface, mut presenter) = setup();
        let policy = UpdatePolicy::default();
        presenter.have_frame = true;
        assert!(presenter.issue_due(&mut surface, &policy, false));
        screen.take_calls();
        assert_eq!(presenter.next_deadline(), Some(clock.now() + VERIFY_DELAY));

        clock.advance(VERIFY_DELAY - Duration::from_millis(1));
        presenter.poll_verification(clock.now());
        assert!(!presenter.issue_due(&mut surface, &policy, false));

        clock.advance(Duration::from_millis(1));
        presenter.poll_verification(clock.now());
        assert!(presenter.issue_due(&mut surface, &policy, false));
        assert_eq!(screen.take_calls(), present_calls());
        assert!(matches!(presenter.state, PresentState::Idle));
    }

    #[test]
    fn present_waits_for_a_busy_panel_at_most_1200ms() {
        let (clock, screen, mut surface, mut presenter) = setup();
        let policy = UpdatePolicy::default();
        presenter.have_frame = true;
        screen.set_busy(true);

        let started = clock.now();
        while clock.now() - started < PRESENT_WAIT_BUDGET {
            assert!(!presenter.issue_due(&mut surface, &policy, false));
            // Not due again before the next 20 ms retry.
            assert!(!presenter.issue_due(&mut surface, &policy, false));
            clock.advance(PRESENT_RETRY);
        }
        assert!(screen.take_calls().is_empty());
        assert!(presenter.issue_due(&mut surface, &policy, false));
        assert_eq!(screen.take_calls(), present_calls());
    }

    #[test]
    fn present_goes_out_as_soon_as_the_panel_is_idle() {
        let (clock, screen, mut surface, mut presenter) = setup();
        let policy = UpdatePolicy::default();
        presenter.have_frame = true;
        screen.set_busy(true);
        assert!(!presenter.issue_due(&mut surface, &policy, false));
        clock.advance(PRESENT_RETRY);
        screen.set_busy(false);
        assert!(presenter.issue_due(&mut surface, &policy, false));
        assert_eq!(screen.take_calls(), present_calls());
    }
}
