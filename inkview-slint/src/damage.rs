//! The content damage path: partial updates while the panel is idle, dynamic
//! updates while it is busy, and a finalizer that sends the waiting damage
//! once the panel is idle again.

use std::time::{Duration, Instant};

use crate::clock::Clock;
use crate::eink_screen::EinkScreen;
use crate::policy::UpdatePolicy;
use crate::surface::{Rect, Surface, fmt_rect};

/// Damage above this while the panel is busy is treated as a card/text
/// transition: do not use DynamicUpdate. Button animations and other small
/// feedback are well below this and stay on the fast dynamic path.
const LARGE_DAMAGE_AREA: i32 = 100_000;

/// The finalizer waits this long after the last draw.
const FINALIZE_AFTER: Duration = Duration::from_millis(200);

/// Retry interval of the finalizer while the panel is busy.
const FINALIZE_RETRY: Duration = Duration::from_millis(20);

/// After this many busy retries, the finalizer uses the fallback update.
const FINALIZE_MAX_RETRIES: u32 = 100;

/// Minimum time since the last draw for a dynamic update.
const DYNAMIC_MIN_INTERVAL: Duration = Duration::from_millis(20);

pub(crate) struct DamagePath {
    clock: Clock,
    /// When the finalizer runs next.
    fulfill_after: Option<Instant>,
    /// Damage since the panel turned busy, for the finalizer.
    dynamic_region: Option<Rect>,
    /// Damage not yet sent as a dynamic update.
    accumulated: Option<Rect>,
    finalize_retries: u32,
    last_draw_at: Instant,
}

impl DamagePath {
    pub fn new(clock: Clock) -> Self {
        let last_draw_at = clock.now();
        Self {
            clock,
            fulfill_after: None,
            dynamic_region: None,
            accumulated: None,
            finalize_retries: 0,
            last_draw_at,
        }
    }

    /// Time until the finalizer is due.
    pub fn deadline(&self, now: Instant) -> Option<Duration> {
        self.fulfill_after
            .map(|at| at.saturating_duration_since(now))
    }

    /// Drops the waiting damage; a full present covers it.
    pub fn drop_waiting(&mut self) {
        self.dynamic_region = None;
        self.accumulated = None;
        self.fulfill_after = None;
        self.finalize_retries = 0;
    }

    /// After a full present: drops the waiting damage and counts the present
    /// as the last draw.
    pub fn presented(&mut self) {
        self.drop_waiting();
        self.last_draw_at = self.clock.now();
    }

    fn since_last_draw(&self) -> Duration {
        self.clock
            .now()
            .saturating_duration_since(self.last_draw_at)
    }

    /// The finalizer: once the last draw is older than 200 ms, sends the
    /// waiting damage as one update when the panel is idle. While the panel
    /// is busy it retries every 20 ms, and after 100 retries it uses the
    /// fallback update.
    pub fn finalize<S: EinkScreen>(&mut self, surface: &mut Surface<S>, policy: &UpdatePolicy) {
        let Some(redraw_region) = self.dynamic_region else {
            return;
        };
        if self.since_last_draw() <= FINALIZE_AFTER {
            return;
        }

        if surface.is_updating() {
            // The panel is still busy. Keep the pending region
            // instead of dropping the final frame; retry shortly.
            self.finalize_retries += 1;
            if self.finalize_retries > FINALIZE_MAX_RETRIES {
                // The panel appears stuck; recover with a full
                // refresh from the current framebuffer.
                eink_log!(
                    "finalize fallback: {} {}",
                    policy.fallback,
                    fmt_rect(&redraw_region)
                );
                surface.issue(policy.fallback, &redraw_region);
                surface.wake();
                self.drop_waiting();
                self.last_draw_at = self.clock.now();
            } else {
                self.fulfill_after = Some(self.clock.now() + FINALIZE_RETRY);
            }
        } else {
            eink_log!(
                "update finalize {} retries={}",
                fmt_rect(&redraw_region),
                self.finalize_retries
            );
            self.drop_waiting();

            if !redraw_region.is_empty() {
                surface.issue(policy.finalize, &redraw_region);
                surface.wake();
            }
            self.last_draw_at = self.clock.now();
        }
    }

    /// Sends rendered damage (content coordinates, not empty) that no full
    /// present covers: copies it to the framebuffer, then a partial update
    /// when the panel is idle, or a dynamic update when it is busy and the
    /// accumulated damage is small. Busy damage also waits for the finalizer.
    pub fn submit<S: EinkScreen>(
        &mut self,
        surface: &mut Surface<S>,
        policy: &UpdatePolicy,
        damage: Rect,
        render: Duration,
    ) {
        surface.copy_damage(&damage);

        // Damage size selects only the dynamic/finalizer policy
        // below. Genuine screen replacements are requested by the
        // application instead of being inferred from damage.
        if surface.is_updating() {
            let accumulated = match self.accumulated {
                Some(r) => r.union(&damage),
                None => damage,
            };
            self.accumulated = Some(accumulated);

            if accumulated.width() * accumulated.height() <= LARGE_DAMAGE_AREA
                && self.since_last_draw() > DYNAMIC_MIN_INTERVAL
            {
                self.accumulated = None;
                eink_log!(
                    "update dynamic {} render={}ms (panel busy)",
                    fmt_rect(&accumulated),
                    render.as_millis()
                );
                surface.issue(policy.busy_content, &accumulated);
                surface.wake();
                self.last_draw_at = self.clock.now();
            } else {
                eink_log!(
                    "damage {} render={}ms deferred to finalize (panel busy)",
                    fmt_rect(&damage),
                    render.as_millis()
                );
            }

            self.dynamic_region = Some(match self.dynamic_region {
                Some(r) => r.union(&damage),
                None => damage,
            });
            self.fulfill_after = Some(self.clock.now() + FINALIZE_AFTER);
        } else {
            eink_log!(
                "update partial {} render={}ms",
                fmt_rect(&damage),
                render.as_millis()
            );
            self.accumulated = None;
            surface.issue(policy.idle_content, &damage);
            surface.wake();
            self.last_draw_at = self.clock.now();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock_screen::{Call, MockScreen};
    use inkview::screen::UpdateKind;

    const TOP: usize = 20;

    fn setup() -> (Clock, MockScreen, Surface<MockScreen>, DamagePath) {
        let clock = Clock::manual();
        let screen =
            MockScreen::new(1000, 1000).with_update_time(&clock, Duration::from_millis(400));
        screen.state.borrow_mut().panel_height = TOP;
        let surface = Surface::new(screen.clone());
        let path = DamagePath::new(clock.clone());
        (clock, screen, surface, path)
    }

    fn rect(x: i32, y: i32, w: i32, h: i32) -> Rect {
        Rect::new(euclid::Point2D::new(x, y), euclid::Size2D::new(w, h))
    }

    fn copy(r: Rect) -> Call {
        Call::CopyRect {
            x: r.origin.x as usize,
            y: r.origin.y as usize + TOP,
            w: r.width() as usize,
            h: r.height() as usize,
        }
    }

    fn update(kind: UpdateKind, r: Rect) -> Call {
        Call::Update {
            kind,
            x: r.origin.x,
            y: r.origin.y + TOP as i32,
            w: r.width() as u32,
            h: r.height() as u32,
        }
    }

    const MS: fn(u64) -> Duration = Duration::from_millis;

    #[test]
    fn idle_damage_is_one_partial_update() {
        let (_clock, screen, mut surface, mut path) = setup();
        let damage = rect(10, 20, 300, 400);
        path.submit(&mut surface, &UpdatePolicy::default(), damage, MS(5));
        assert_eq!(
            screen.take_calls(),
            [
                copy(damage),
                update(UpdateKind::Partial, damage),
                Call::Wake
            ]
        );
        assert_eq!(path.deadline(Instant::now()), None);
    }

    #[test]
    fn small_busy_damage_is_dynamic_then_finalized() {
        let (clock, screen, mut surface, mut path) = setup();
        let policy = UpdatePolicy::default();
        let first = rect(0, 0, 200, 200);
        path.submit(&mut surface, &policy, first, MS(5));
        screen.take_calls();

        // Panel busy (400 ms per update), 20+ ms after the last draw.
        clock.advance(MS(30));
        let small = rect(50, 60, 100, 100);
        path.submit(&mut surface, &policy, small, MS(5));
        assert_eq!(
            screen.take_calls(),
            [copy(small), update(UpdateKind::Dynamic, small), Call::Wake]
        );
        assert_eq!(path.deadline(clock.now()), Some(FINALIZE_AFTER));

        // Not before 200 ms after the last draw.
        clock.advance(FINALIZE_AFTER);
        path.finalize(&mut surface, &policy);
        assert!(screen.take_calls().is_empty());

        // Busy: retry in 20 ms; no update.
        clock.advance(MS(1));
        path.finalize(&mut surface, &policy);
        assert!(screen.take_calls().is_empty());
        assert_eq!(path.deadline(clock.now()), Some(FINALIZE_RETRY));

        // Idle: one partial of the waiting damage.
        clock.advance(MS(400));
        path.finalize(&mut surface, &policy);
        assert_eq!(
            screen.take_calls(),
            [update(UpdateKind::Partial, small), Call::Wake]
        );
        assert_eq!(path.deadline(clock.now()), None);
        path.finalize(&mut surface, &policy);
        assert!(screen.take_calls().is_empty());
    }

    #[test]
    fn large_busy_damage_waits_for_the_finalizer() {
        let (clock, screen, mut surface, mut path) = setup();
        let policy = UpdatePolicy::default();
        screen.set_busy(true);
        clock.advance(MS(30));
        let large = rect(0, 0, 400, 300);
        path.submit(&mut surface, &policy, large, MS(5));
        assert_eq!(screen.take_calls(), [copy(large)]);

        screen.set_busy(false);
        clock.advance(FINALIZE_AFTER);
        path.finalize(&mut surface, &policy);
        assert_eq!(
            screen.take_calls(),
            [update(UpdateKind::Partial, large), Call::Wake]
        );
    }

    #[test]
    fn busy_damage_within_20ms_of_a_draw_is_deferred() {
        let (_clock, screen, mut surface, mut path) = setup();
        screen.set_busy(true);
        let small = rect(0, 0, 10, 10);
        path.submit(&mut surface, &UpdatePolicy::default(), small, MS(5));
        assert_eq!(screen.take_calls(), [copy(small)]);
    }

    #[test]
    fn stuck_panel_gets_the_fallback_after_100_retries() {
        let (clock, screen, mut surface, mut path) = setup();
        let policy = UpdatePolicy::default();
        screen.set_busy(true);
        clock.advance(MS(30));
        let large = rect(0, 0, 400, 300);
        path.submit(&mut surface, &policy, large, MS(5));
        screen.take_calls();

        clock.advance(FINALIZE_AFTER + MS(1));
        for _ in 0..FINALIZE_MAX_RETRIES {
            path.finalize(&mut surface, &policy);
        }
        assert!(screen.take_calls().is_empty());
        path.finalize(&mut surface, &policy);
        assert_eq!(
            screen.take_calls(),
            [update(UpdateKind::Full, large), Call::Wake]
        );
        assert_eq!(path.deadline(clock.now()), None);
    }

    #[test]
    fn empty_waiting_damage_sends_no_update() {
        let (clock, screen, mut surface, mut path) = setup();
        path.dynamic_region = Some(rect(0, 0, 0, 0));
        clock.advance(FINALIZE_AFTER + MS(1));
        path.finalize(&mut surface, &UpdatePolicy::default());
        assert!(screen.take_calls().is_empty());
        assert_eq!(path.dynamic_region, None);
    }

    #[test]
    fn a_present_drops_the_waiting_damage() {
        let (clock, screen, mut surface, mut path) = setup();
        let policy = UpdatePolicy::default();
        screen.set_busy(true);
        clock.advance(MS(30));
        path.submit(&mut surface, &policy, rect(0, 0, 400, 300), MS(5));
        screen.take_calls();

        path.presented();
        assert_eq!(path.deadline(clock.now()), None);
        screen.set_busy(false);
        clock.advance(FINALIZE_AFTER + MS(1));
        path.finalize(&mut surface, &policy);
        assert!(screen.take_calls().is_empty());
    }
}
