use inkview::Event as InkEvent;
use inkview::event::Key;
use inkview::screen::{PixelFormat, Screen};
use slint::platform::{
    EventLoopProxy, WindowEvent,
    software_renderer::{self as renderer, PhysicalRegion, PremultipliedRgbaColor, TargetPixel},
};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::{
        OnceLock,
        atomic::{AtomicBool, Ordering},
        mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError},
    },
    time::{Duration, Instant},
};

/// Used when the screen reports no usable DPI/scale (see `scale_from_screen`).
/// The pbanki UI was laid out for 3.0 on a 300 dpi panel.
const DEFAULT_SCALE_FACTOR: f32 = 3.0;

/// Damage above this while the panel is busy is treated as a card/text
/// transition: do not use DynamicUpdate. Button animations and other small
/// feedback are well below this and stay on the fast dynamic path.
const LARGE_DAMAGE_AREA: i32 = 100_000;

/// Delay after an issued full present before verifying it with a second one.
/// The first update can be discarded while InkView is still settling;
/// verification does not delay the first frame.
const VERIFY_DELAY: Duration = Duration::from_millis(300);

/// Longest a full present waits for the panel to report idle. After this the
/// non-flashing update is issued anyway instead of falling back to a flashing
/// `FullUpdate`.
const PRESENT_WAIT_BUDGET: Duration = Duration::from_millis(1200);

/// Poll interval while a full present waits for the panel.
const PRESENT_RETRY: Duration = Duration::from_millis(20);

/// While the panel reports busy, wake at this interval to sample it so the
/// busy→idle log line carries an accurate timestamp.
const PANEL_POLL: Duration = Duration::from_millis(50);
/// Slower sampling once the panel has been busy for longer than this.
const PANEL_POLL_SLOW_AFTER: Duration = Duration::from_secs(10);
const PANEL_POLL_SLOW: Duration = Duration::from_millis(500);

static FULL_PRESENT_REQUESTED: AtomicBool = AtomicBool::new(false);
static EPOCH: OnceLock<Instant> = OnceLock::new();

/// Requests a non-flashing, full-frame `PartialUpdate` after the current Slint
/// damage has been rendered. This does not invoke InkView's flashing
/// `FullUpdate` and is intended only for genuine screen transitions.
///
/// Calls are coalesced. The hook used by pbanki calls this from the UI thread;
/// the flag itself does not wake an event loop blocked in another thread.
pub fn request_full_present() {
    FULL_PRESENT_REQUESTED.store(true, Ordering::Release);
}

fn since_epoch_ms() -> u128 {
    EPOCH.get_or_init(Instant::now).elapsed().as_millis()
}

macro_rules! eink_log {
    ($($arg:tt)*) => {
        eprintln!("[eink +{}] {}", since_epoch_ms(), format_args!($($arg)*))
    };
}

/// Render target in the task framebuffer's own pixel format (`BB8` on
/// grayscale panels, `RGB24` on colour panels), so frames can be copied row by
/// row without per-pixel conversion.
#[repr(transparent)]
#[derive(Clone, Copy, Default)]
struct Pixel<P>(P);

impl<P: PixelFormat> TargetPixel for Pixel<P> {
    fn blend(&mut self, color: PremultipliedRgbaColor) {
        let src = P::from_rgb24(color.red, color.green, color.blue);
        self.0 = self.0.mix(src, u8::MAX - color.alpha);
    }

    fn from_rgb(red: u8, green: u8, blue: u8) -> Self {
        Self(P::from_rgb24(red, green, blue))
    }
}

fn pixels<P>(buffer: &[Pixel<P>]) -> &[P] {
    // SAFETY: `Pixel<P>` is `repr(transparent)` over `P`.
    unsafe { core::slice::from_raw_parts(buffer.as_ptr().cast::<P>(), buffer.len()) }
}

/// Events for the backend's event loop.
pub enum Event {
    InkviewEvent(inkview::Event),
    /// Runs on the Slint thread; sent by `slint::invoke_from_event_loop`.
    Callback(Box<dyn FnOnce() + Send>),
    /// Sent by `slint::quit_event_loop`.
    Quit,
}

struct Proxy {
    queue: Sender<Event>,
}

impl EventLoopProxy for Proxy {
    fn quit_event_loop(&self) -> Result<(), slint::EventLoopError> {
        self.queue
            .send(Event::Quit)
            .map_err(|_| slint::EventLoopError::EventLoopTerminated)
    }

    fn invoke_from_event_loop(
        &self,
        event: Box<dyn FnOnce() + Send>,
    ) -> Result<(), slint::EventLoopError> {
        self.queue
            .send(Event::Callback(event))
            .map_err(|_| slint::EventLoopError::EventLoopTerminated)
    }
}

/// `dpi / 100 × scale` from InkView, or `DEFAULT_SCALE_FACTOR` if that is out
/// of the plausible range (some firmware reports 0).
fn scale_from_screen<P>(screen: &Screen<P>) -> f32 {
    let scale = screen.dpi() as f32 / 100.0 * screen.scale();
    if (1.5..=6.0).contains(&scale) {
        scale
    } else {
        DEFAULT_SCALE_FACTOR
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PresentReason {
    Launch,
    Show,
    Foreground,
    Repaint,
    App,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PresentKind {
    /// A requested present; followed by one verification.
    Primary,
    /// The verification of an earlier present; not verified again.
    Verify,
}

#[derive(Clone, Copy, Debug)]
enum PresentState {
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
struct Presenter {
    state: PresentState,
    visible: bool,
    /// The retained buffer holds a complete frame.
    have_frame: bool,
    /// Last sampled `IsUpdateInProcess()` and when it turned busy.
    panel_busy_since: Option<Instant>,
}

impl Presenter {
    fn new() -> Self {
        let now = Instant::now();
        eink_log!("request reason=Launch");
        Self {
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

    fn is_pending(&self) -> bool {
        matches!(self.state, PresentState::Pending { .. })
    }

    /// Returns true if this started a new pending present (rather than
    /// coalescing into one), so callers can drop competing damage updates.
    fn request(&mut self, new_reason: PresentReason) -> bool {
        let now = Instant::now();
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

    fn hide(&mut self) {
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
    fn poll_verification(&mut self, now: Instant) {
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

    fn next_deadline(&self) -> Option<Instant> {
        match self.state {
            PresentState::Idle => None,
            PresentState::Pending { next_attempt, .. } => Some(next_attempt),
            PresentState::Verifying { due } => Some(due),
        }
    }

    /// Samples the panel and logs busy→idle transitions.
    fn sample_panel<P>(&mut self, screen: &mut Screen<'static, P>) -> bool {
        let busy = screen.is_updating();
        match (busy, self.panel_busy_since) {
            (true, None) => self.panel_busy_since = Some(Instant::now()),
            (false, Some(since)) => {
                eink_log!("panel busy→idle after {}ms", since.elapsed().as_millis());
                self.panel_busy_since = None;
            }
            _ => {}
        }
        busy
    }

    fn panel_poll_interval(&self) -> Option<Duration> {
        self.panel_busy_since.map(|since| {
            if since.elapsed() > PANEL_POLL_SLOW_AFTER {
                PANEL_POLL_SLOW
            } else {
                PANEL_POLL
            }
        })
    }
}

pub struct Backend<P: 'static> {
    screen: RefCell<Screen<'static, P>>,
    sender: Sender<Event>,
    receiver: Receiver<Event>,
    width: usize,
    height: usize,
    scale_factor: f32,
    window: RefCell<Option<Rc<renderer::MinimalSoftwareWindow>>>,
    buffer: RefCell<Vec<Pixel<P>>>,
    trace_events: bool,
    wake_logged: Cell<bool>,
}

impl<P: PixelFormat + Default> Backend<P> {
    /// Create a new backend. `sender` and `receiver` must be of the same
    /// channel; InkView events are sent into it as `Event::InkviewEvent`.
    pub fn new(
        screen: Screen<'static, P>,
        sender: Sender<Event>,
        receiver: Receiver<Event>,
    ) -> Self {
        let width = screen.width();
        let height = screen.height();
        let scale_factor = scale_from_screen(&screen);

        EPOCH.get_or_init(Instant::now);
        let trace_events = std::env::var_os("INKVIEW_SLINT_TRACE_EVENTS").is_some();
        eink_log!(
            "inkview-slint patched backend (pbanki-upstream-master, presenter) \
             {width}x{height} {}-byte pixels scale={scale_factor} (dpi={} screen scale={}){}",
            P::BYTES,
            screen.dpi(),
            screen.scale(),
            if trace_events { ", tracing events" } else { "" }
        );

        let buffer = vec![Pixel::<P>::default(); width * height];

        Self {
            screen: screen.into(),
            sender,
            receiver,
            width,
            height,
            scale_factor,
            window: Default::default(),
            buffer: buffer.into(),
            trace_events,
            wake_logged: Cell::new(false),
        }
    }

    /// Access the event sender; clone it to send in InkView events.
    pub fn event_sender(&self) -> &Sender<Event> {
        &self.sender
    }

    /// Wakes InkView's main loop after a screen update.
    ///
    /// Updates are requested from the Slint thread while `InkViewMain` runs on
    /// the main thread. On the PB710 an update could stay unprocessed (panel
    /// reported idle after ~100 ms, nothing drawn) until the next touch woke
    /// that loop. See `Screen::wake_event_loop`.
    fn wake(&self, screen: &Screen<'static, P>) {
        let rc = screen.wake_event_loop();
        if !self.wake_logged.replace(true) {
            eink_log!("waking InkView loop after updates: SendEventTo(EVT_CUSTOM) rc={rc:?}");
        }
    }

    /// Copy the retained frame into the task framebuffer.
    ///
    /// InkView can blank or replace the task framebuffer while another task is
    /// in front. Re-blitting the retained frame before a full-screen update
    /// makes a show/resume independent of whatever currently sits in that
    /// buffer.
    fn blit_all(&self, screen: &mut Screen<'static, P>) {
        let buffer = self.buffer.borrow();
        screen.copy_rect(0, 0, self.width, self.height, pixels(&buffer), self.width);
    }
}

/// Compact `WxH+X+Y` form for logs.
fn fmt_rect(r: &euclid::Rect<i32, euclid::UnknownUnit>) -> String {
    format!("{}x{}+{}+{}", r.width(), r.height(), r.origin.x, r.origin.y)
}

fn rect_from_phys(r: &PhysicalRegion) -> euclid::Rect<i32, euclid::UnknownUnit> {
    euclid::Rect::new(
        euclid::Point2D::new(r.bounding_box_origin().x, r.bounding_box_origin().y),
        euclid::Size2D::new(
            r.bounding_box_size().width as i32,
            r.bounding_box_size().height as i32,
        ),
    )
}

impl<P: PixelFormat + Default> slint::platform::Platform for Backend<P> {
    fn create_window_adapter(
        &self,
    ) -> Result<Rc<dyn slint::platform::WindowAdapter>, slint::PlatformError> {
        let window =
            renderer::MinimalSoftwareWindow::new(renderer::RepaintBufferType::ReusedBuffer);
        self.window.replace(Some(window.clone()));
        Ok(window)
    }

    fn new_event_loop_proxy(&self) -> Option<Box<dyn EventLoopProxy>> {
        Some(Box::new(Proxy {
            queue: self.sender.clone(),
        }))
    }

    fn run_event_loop(&self) -> Result<(), slint::PlatformError> {
        let scale_factor = self.scale_factor;
        slint::Window::set_size(
            self.window.borrow().as_ref().unwrap().as_ref(),
            slint::PhysicalSize::new(self.width as u32, self.height as u32)
                .to_logical(scale_factor),
        );

        self.window
            .borrow()
            .as_ref()
            .unwrap()
            .dispatch_event(WindowEvent::ScaleFactorChanged { scale_factor });

        // Normal damage path: dynamic updates while the panel is busy, then a
        // final partial update of the accumulated region once it settles.
        let mut fulfill_dynamic_updates_after: Option<Instant> = None;
        let mut dynamic_region_to_redraw: Option<euclid::Rect<i32, euclid::UnknownUnit>> = None;
        let mut accumulated_updates: Option<euclid::Rect<i32, euclid::UnknownUnit>> = None;
        let mut dynamic_finalize_retries: u32 = 0;
        let mut last_draw_at = Instant::now();

        let mut presenter = Presenter::new();
        let mut force_full_render = false;
        let mut events: Vec<Event> = Vec::new();

        loop {
            slint::platform::update_timers_and_animations();

            let Some(window) = self.window.borrow().clone() else {
                continue;
            };

            presenter.sample_panel(&mut self.screen.borrow_mut());

            let delay = if FULL_PRESENT_REQUESTED.load(Ordering::Acquire) {
                // A timer may have requested a transition earlier in this
                // iteration. Do not block before consuming that request.
                Some(Duration::ZERO)
            } else if window.has_active_animations() {
                None
            } else {
                let now = Instant::now();
                [
                    slint::platform::duration_until_next_timer_update(),
                    fulfill_dynamic_updates_after.map(|i| i.saturating_duration_since(now)),
                    presenter
                        .next_deadline()
                        .map(|i| i.saturating_duration_since(now)),
                    presenter.panel_poll_interval(),
                ]
                .into_iter()
                .flatten()
                .min()
                .or(Some(Duration::from_millis(1000)))
            };

            // Block for the first event, then drain everything already queued
            // so back-to-back Show/Foreground produce a single present.
            events.clear();
            let first = match delay {
                Some(delay) => match self.receiver.recv_timeout(delay) {
                    Ok(evt) => Some(evt),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => return Ok(()),
                },
                None => match self.receiver.try_recv() {
                    Ok(evt) => Some(evt),
                    Err(TryRecvError::Empty) => None,
                    Err(TryRecvError::Disconnected) => return Ok(()),
                },
            };
            if let Some(evt) = first {
                events.push(evt);
                while let Ok(evt) = self.receiver.try_recv() {
                    events.push(evt);
                }
            }

            if let Some(redraw_region) = dynamic_region_to_redraw
                && last_draw_at.elapsed() > Duration::from_millis(200)
            {
                let mut screen = self.screen.borrow_mut();

                if screen.is_updating() {
                    // The panel is still busy. Keep the pending region
                    // instead of dropping the final frame; retry shortly.
                    dynamic_finalize_retries += 1;
                    if dynamic_finalize_retries > 100 {
                        // The panel appears stuck; recover with a full
                        // refresh from the current framebuffer.
                        eink_log!("finalize fallback: FullUpdate {}", fmt_rect(&redraw_region));
                        screen.full_update();
                        self.wake(&screen);
                        dynamic_region_to_redraw = None;
                        accumulated_updates = None;
                        fulfill_dynamic_updates_after = None;
                        dynamic_finalize_retries = 0;
                        last_draw_at = Instant::now();
                    } else {
                        fulfill_dynamic_updates_after =
                            Some(Instant::now() + Duration::from_millis(20));
                    }
                } else {
                    eink_log!(
                        "update finalize {} retries={dynamic_finalize_retries}",
                        fmt_rect(&redraw_region)
                    );
                    dynamic_region_to_redraw = None;
                    accumulated_updates = None;
                    fulfill_dynamic_updates_after = None;
                    dynamic_finalize_retries = 0;

                    screen.partial_update(
                        redraw_region.origin.x,
                        redraw_region.origin.y,
                        redraw_region.width() as u32,
                        redraw_region.height() as u32,
                    );
                    self.wake(&screen);
                    last_draw_at = Instant::now();
                }
            }

            let mut new_present = false;
            for evt in events.drain(..) {
                // Callbacks run in queue order with InkView events, before
                // timers and the full-present latch below, so a callback that
                // requests a present is rendered in this iteration.
                let evt = match evt {
                    Event::InkviewEvent(evt) => evt,
                    Event::Callback(callback) => {
                        callback();
                        continue;
                    }
                    Event::Quit => return Ok(()),
                };
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
                match evt {
                    InkEvent::Show | InkEvent::Repaint | InkEvent::Foreground { .. } => {
                        presenter.visible = true;
                        let reason = match evt {
                            InkEvent::Show => PresentReason::Show,
                            InkEvent::Repaint => PresentReason::Repaint,
                            _ => PresentReason::Foreground,
                        };
                        new_present |= presenter.request(reason);
                        window.request_redraw();
                    }
                    InkEvent::Background { .. } | InkEvent::Hide => {
                        // Do not present into a task that is no longer visible.
                        presenter.hide();
                        force_full_render = false;
                    }
                    _ => {}
                }

                if let Some(evt) = ink_evt_to_slint(scale_factor, evt) {
                    window.dispatch_event(evt);
                }
            }

            // After dispatch, so change handlers queued by input (Slint runs
            // `changed` callbacks here) take effect in this iteration's render
            // instead of after the next wakeup.
            slint::platform::update_timers_and_animations();

            if FULL_PRESENT_REQUESTED.swap(false, Ordering::AcqRel) && presenter.visible {
                // Consume after timers/event dispatch but before rendering,
                // so the full present contains the screen that requested it
                // and the normal damage path does not race it.
                new_present |= presenter.request(PresentReason::App);
                window.request_redraw();
            }

            presenter.poll_verification(Instant::now());

            if new_present {
                // The full present below covers the whole frame; drop any
                // pending dynamic presentation.
                dynamic_region_to_redraw = None;
                accumulated_updates = None;
                fulfill_dynamic_updates_after = None;
                dynamic_finalize_retries = 0;
            }

            if presenter.is_pending() && !presenter.have_frame {
                // No complete frame yet. Force one full render so the re-blit
                // below has valid pixels.
                force_full_render = true;
                window.request_redraw();
            }

            window.draw_if_needed(|renderer| {
                let full = force_full_render;
                if full {
                    renderer.set_repaint_buffer_type(renderer::RepaintBufferType::NewBuffer);
                }
                let render_started = Instant::now();
                let mut buffer = self.buffer.borrow_mut();
                let damage = renderer.render(buffer.as_mut_slice(), self.width);
                if full {
                    renderer.set_repaint_buffer_type(renderer::RepaintBufferType::ReusedBuffer);
                    force_full_render = false;
                }
                let render_elapsed = render_started.elapsed();

                let damage_rect = rect_from_phys(&damage);
                let full_damage = damage_rect.width() >= self.width as i32
                    && damage_rect.height() >= self.height as i32;
                if !presenter.have_frame && (full || full_damage) {
                    presenter.have_frame = true;
                    eink_log!(
                        "first full frame rendered in {}ms",
                        render_elapsed.as_millis()
                    );
                } else if render_elapsed > Duration::from_millis(500) {
                    eink_log!(
                        "slow render: {}ms damage={damage_rect:?}",
                        render_elapsed.as_millis()
                    );
                }

                if presenter.is_pending() {
                    // The pending full present re-blits and presents the whole
                    // retained frame; damage only updates the retained buffer.
                    eink_log!("damage {} held for pending present", fmt_rect(&damage_rect));
                    return;
                }

                let mut screen = self.screen.borrow_mut();
                let origin = damage.bounding_box_origin();
                let size = damage.bounding_box_size();
                screen.copy_rect(
                    origin.x.max(0) as usize,
                    origin.y.max(0) as usize,
                    size.width as usize,
                    size.height as usize,
                    pixels(&buffer),
                    self.width,
                );
                drop(buffer);

                // Damage size selects only the dynamic/finalizer policy
                // below. Genuine screen replacements are requested by the
                // application instead of being inferred from damage.
                if screen.is_updating() {
                    if let Some(r) = accumulated_updates.as_mut() {
                        *r = r.union(&damage_rect);
                    } else {
                        accumulated_updates = Some(damage_rect);
                    }

                    let accumulated_area = accumulated_updates
                        .as_ref()
                        .map(|r| r.width() * r.height())
                        .unwrap_or(0);

                    if accumulated_area <= LARGE_DAMAGE_AREA
                        && last_draw_at.elapsed() > Duration::from_millis(20)
                    {
                        let redraw_region = accumulated_updates.take().unwrap();
                        eink_log!(
                            "update dynamic {} render={}ms (panel busy)",
                            fmt_rect(&redraw_region),
                            render_elapsed.as_millis()
                        );
                        screen.dynamic_update(
                            redraw_region.origin.x,
                            redraw_region.origin.y,
                            redraw_region.width() as u32,
                            redraw_region.height() as u32,
                        );
                        self.wake(&screen);
                        last_draw_at = Instant::now();
                    } else {
                        eink_log!(
                            "damage {} render={}ms deferred to finalize (panel busy)",
                            fmt_rect(&damage_rect),
                            render_elapsed.as_millis()
                        );
                    }

                    if let Some(r) = dynamic_region_to_redraw.as_mut() {
                        *r = r.union(&damage_rect);
                    } else {
                        dynamic_region_to_redraw = Some(damage_rect);
                    }

                    fulfill_dynamic_updates_after =
                        Some(Instant::now() + Duration::from_millis(200));
                } else {
                    eink_log!(
                        "update partial {} render={}ms",
                        fmt_rect(&damage_rect),
                        render_elapsed.as_millis()
                    );
                    accumulated_updates = None;
                    screen.partial_update(origin.x, origin.y, size.width, size.height);
                    self.wake(&screen);
                    last_draw_at = Instant::now();
                }
            });

            if let PresentState::Pending {
                kind,
                reason,
                since,
                attempts,
                waiting_since,
                next_attempt,
            } = &mut presenter.state
            {
                let now = Instant::now();
                if now >= *next_attempt && !force_full_render && presenter.have_frame {
                    let (kind, reason, since) = (*kind, *reason, *since);
                    let mut screen = self.screen.borrow_mut();
                    let busy = screen.is_updating();
                    let waited = waiting_since.map(|at| now - at).unwrap_or_default();

                    if busy && waited < PRESENT_WAIT_BUDGET {
                        if waiting_since.is_none() {
                            *waiting_since = Some(now);
                        }
                        *attempts += 1;
                        *next_attempt = now + PRESENT_RETRY;
                    } else {
                        let attempts = *attempts;
                        let blit_started = Instant::now();
                        self.blit_all(&mut screen);
                        let blit_elapsed = blit_started.elapsed();
                        screen.partial_update(0, 0, self.width as u32, self.height as u32);
                        self.wake(&screen);
                        drop(screen);

                        match kind {
                            PresentKind::Primary => {
                                eink_log!(
                                    "issued reason={reason:?} after={}ms waited={}ms \
                                     attempts={attempts} busy={busy} blit={}ms",
                                    (now - since).as_millis(),
                                    waited.as_millis(),
                                    blit_elapsed.as_millis()
                                );
                                presenter.state = PresentState::Verifying {
                                    due: Instant::now() + VERIFY_DELAY,
                                };
                            }
                            PresentKind::Verify => {
                                eink_log!(
                                    "verify issued waited={}ms attempts={attempts} busy={busy}",
                                    waited.as_millis()
                                );
                                presenter.state = PresentState::Idle;
                            }
                        }
                        // The issued update makes the panel busy again.
                        presenter.sample_panel(&mut self.screen.borrow_mut());
                        dynamic_region_to_redraw = None;
                        accumulated_updates = None;
                        fulfill_dynamic_updates_after = None;
                        dynamic_finalize_retries = 0;
                        last_draw_at = Instant::now();
                    }
                }
            }
        }
    }
}

fn ink_key_to_slint(key: Key) -> Option<slint::platform::Key> {
    match key {
        Key::Up => Some(slint::platform::Key::UpArrow),
        Key::Down => Some(slint::platform::Key::DownArrow),
        Key::Left => Some(slint::platform::Key::LeftArrow),
        Key::Prev => Some(slint::platform::Key::LeftArrow),
        Key::Prev2 => Some(slint::platform::Key::LeftArrow),
        Key::Right => Some(slint::platform::Key::RightArrow),
        Key::Next => Some(slint::platform::Key::RightArrow),
        Key::Next2 => Some(slint::platform::Key::RightArrow),
        Key::Ok => Some(slint::platform::Key::Return),
        Key::Back => Some(slint::platform::Key::Backspace),
        Key::Menu => Some(slint::platform::Key::Menu),
        Key::Home => Some(slint::platform::Key::Home),
        Key::Plus => Some(slint::platform::Key::PageUp),
        Key::Minus => Some(slint::platform::Key::PageDown),
        _ => None,
    }
}

fn ink_evt_to_slint(scale_factor: f32, evt: InkEvent) -> Option<WindowEvent> {
    let evt = match evt {
        InkEvent::PointerDown { x, y } => WindowEvent::PointerPressed {
            position: slint::PhysicalPosition { x, y }.to_logical(scale_factor),
            button: slint::platform::PointerEventButton::Left,
        },
        InkEvent::PointerMove { x, y } => WindowEvent::PointerMoved {
            position: slint::PhysicalPosition { x, y }.to_logical(scale_factor),
        },
        InkEvent::PointerUp { x, y } => WindowEvent::PointerReleased {
            position: slint::PhysicalPosition { x, y }.to_logical(scale_factor),
            button: slint::platform::PointerEventButton::Left,
        },
        InkEvent::Foreground { .. } => WindowEvent::WindowActiveChanged(true),
        InkEvent::Background { .. } => WindowEvent::WindowActiveChanged(false),
        InkEvent::KeyDown { key } => WindowEvent::KeyPressed {
            text: ink_key_to_slint(key)?.into(),
        },
        InkEvent::KeyRepeat { key } => WindowEvent::KeyPressRepeated {
            text: ink_key_to_slint(key)?.into(),
        },
        InkEvent::KeyUp { key } => WindowEvent::KeyReleased {
            text: ink_key_to_slint(key)?.into(),
        },
        _ => return None,
    };

    Some(evt)
}

#[cfg(test)]
mod tests {
    use super::*;
    use inkview::screen::{BB8, RGB24};

    fn color(red: u8, green: u8, blue: u8, alpha: u8) -> PremultipliedRgbaColor {
        PremultipliedRgbaColor {
            red,
            green,
            blue,
            alpha,
        }
    }

    #[test]
    fn gray_from_rgb_uses_luma() {
        assert_eq!(Pixel::<BB8>::from_rgb(255, 255, 255).0, BB8(255));
        assert_eq!(Pixel::<BB8>::from_rgb(0, 0, 0).0, BB8(0));
        assert_eq!(Pixel::<BB8>::from_rgb(128, 128, 128).0, BB8(128));
        assert_eq!(Pixel::<BB8>::from_rgb(255, 0, 0).0, BB8(53));
    }

    #[test]
    fn gray_blend_premultiplied() {
        let mut px = Pixel(BB8(255));
        px.blend(color(0, 0, 0, 255));
        assert_eq!(px.0, BB8(0));

        let mut px = Pixel(BB8(255));
        px.blend(color(0, 0, 0, 0));
        assert_eq!(px.0, BB8(255));

        // 50% black over white.
        let mut px = Pixel(BB8(255));
        px.blend(color(0, 0, 0, 128));
        assert_eq!(px.0, BB8(127));

        // Fully opaque white over black never overflows.
        let mut px = Pixel(BB8(0));
        px.blend(color(255, 255, 255, 255));
        assert_eq!(px.0, BB8(255));
    }

    #[test]
    fn rgb_blend_premultiplied() {
        let mut px = Pixel(RGB24(255, 255, 255));
        px.blend(color(255, 0, 0, 255));
        assert_eq!(px.0, RGB24(255, 0, 0));

        // 50% blue (premultiplied) over white.
        let mut px = Pixel(RGB24(255, 255, 255));
        px.blend(color(0, 0, 128, 128));
        assert_eq!(px.0, RGB24(127, 127, 255));
    }

    #[test]
    fn pixel_buffer_casts_to_formats() {
        let buffer = [Pixel(RGB24(1, 2, 3)), Pixel(RGB24(4, 5, 6))];
        assert_eq!(pixels(&buffer), &[RGB24(1, 2, 3), RGB24(4, 5, 6)]);
    }
}
