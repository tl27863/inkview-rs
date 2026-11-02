use std::{sync::OnceLock, time::Instant};

static EPOCH: OnceLock<Instant> = OnceLock::new();

fn since_epoch_ms() -> u128 {
    EPOCH.get_or_init(Instant::now).elapsed().as_millis()
}

/// Backend log line through the `log` facade (target `eink`), so the
/// application chooses where it goes and at which level.
macro_rules! eink_log {
    ($($arg:tt)*) => {
        log::info!(target: "eink", "[eink +{}] {}", $crate::since_epoch_ms(), format_args!($($arg)*))
    };
}

mod clock;
mod damage;
mod eink_screen;
#[cfg(test)]
mod mock_screen;
mod policy;
mod present;
mod surface;

pub use eink_screen::EinkScreen;
pub use policy::{UPDATES_ENV, UpdateMode, UpdatePolicy};
pub use present::request_full_present;

use clock::Clock;
use damage::DamagePath;
use inkview::Event as InkEvent;
use inkview::event::Key;
use inkview::screen::PixelFormat;
use present::{PresentReason, Presenter, full_present_requested, take_full_present_request};
use slint::platform::{
    EventLoopProxy, WindowEvent,
    software_renderer::{self as renderer, PhysicalRegion, PremultipliedRgbaColor, TargetPixel},
};
use std::{
    cell::RefCell,
    rc::Rc,
    sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError},
    time::Duration,
};
use surface::{Rect, Surface, fmt_rect};

/// Used when the screen reports no usable DPI (see `scale_from_dpi`).
/// The pbanki UI was laid out for 3.0 on a 300 dpi panel.
const DEFAULT_SCALE_FACTOR: f32 = 3.0;

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

/// The retained frame as a Slint render target.
fn target_pixels<P>(frame: &mut [P]) -> &mut [Pixel<P>] {
    // SAFETY: `Pixel<P>` is `repr(transparent)` over `P`.
    unsafe { core::slice::from_raw_parts_mut(frame.as_mut_ptr().cast::<Pixel<P>>(), frame.len()) }
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

/// `dpi / 100`, so a logical pixel has the same physical size on every panel,
/// or `DEFAULT_SCALE_FACTOR` if that is out of the plausible range (some
/// firmware reports 0).
///
/// InkView's screen scale factor is not applied: it carries the firmware's UI
/// zoom (1.16 on a PB710 at 300 dpi), which would enlarge the whole layout.
fn scale_from_dpi(dpi: u32) -> f32 {
    let scale = dpi as f32 / 100.0;
    if (1.5..=6.0).contains(&scale) {
        scale
    } else {
        DEFAULT_SCALE_FACTOR
    }
}

/// Backend settings beyond the screen itself.
#[derive(Clone, Copy, Debug, Default)]
pub struct BackendOptions {
    /// Update mode per class of screen change. `INKVIEW_SLINT_UPDATES`
    /// overrides individual classes on top of this.
    pub policy: UpdatePolicy,
}

pub struct Backend<S: EinkScreen + 'static> {
    surface: RefCell<Surface<S>>,
    sender: Sender<Event>,
    receiver: Receiver<Event>,
    scale_factor: f32,
    policy: UpdatePolicy,
    window: RefCell<Option<Rc<renderer::MinimalSoftwareWindow>>>,
    trace_events: bool,
}

impl<S: EinkScreen> Backend<S> {
    /// Create a new backend. `sender` and `receiver` must be of the same
    /// channel; InkView events are sent into it as `Event::InkviewEvent`.
    pub fn new(screen: S, sender: Sender<Event>, receiver: Receiver<Event>) -> Self {
        Self::with_options(screen, sender, receiver, BackendOptions::default())
    }

    /// Like [`Backend::new`], with explicit options.
    pub fn with_options(
        screen: S,
        sender: Sender<Event>,
        receiver: Receiver<Event>,
        options: BackendOptions,
    ) -> Self {
        let width = screen.width();
        let height = screen.height();
        let scale_factor = scale_from_dpi(screen.dpi());

        EPOCH.get_or_init(Instant::now);
        let trace_events = std::env::var_os("INKVIEW_SLINT_TRACE_EVENTS").is_some();
        eink_log!(
            "inkview-slint patched backend (pbanki-device-panel, presenter) \
             {width}x{height} {}-byte pixels scale={scale_factor} (dpi={} screen scale={}){}",
            S::Pixel::BYTES,
            screen.dpi(),
            screen.scale(),
            if trace_events { ", tracing events" } else { "" }
        );

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
            surface: Surface::new(screen).into(),
            sender,
            receiver,
            scale_factor,
            policy,
            window: Default::default(),
            trace_events,
        }
    }

    /// Sizes the Slint window to the content area.
    fn resize_window(&self) {
        if let Some(window) = self.window.borrow().as_ref() {
            let surface = self.surface.borrow();
            slint::Window::set_size(
                window.as_ref(),
                slint::PhysicalSize::new(surface.width as u32, surface.content_height() as u32)
                    .to_logical(self.scale_factor),
            );
        }
    }

    /// Re-reads the panel height. If it changed, resizes the retained buffer
    /// and the window; returns true so the caller forces a full render.
    fn refresh_panel_height(&self) -> bool {
        if !self.surface.borrow_mut().refresh_panel_height() {
            return false;
        }
        self.resize_window();
        true
    }

    /// Access the event sender; clone it to send in InkView events.
    pub fn event_sender(&self) -> &Sender<Event> {
        &self.sender
    }
}

fn rect_from_phys(r: &PhysicalRegion) -> Rect {
    euclid::Rect::new(
        euclid::Point2D::new(r.bounding_box_origin().x, r.bounding_box_origin().y),
        euclid::Size2D::new(
            r.bounding_box_size().width as i32,
            r.bounding_box_size().height as i32,
        ),
    )
}

impl<S: EinkScreen> slint::platform::Platform for Backend<S> {
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
        self.resize_window();

        self.window
            .borrow()
            .as_ref()
            .unwrap()
            .dispatch_event(WindowEvent::ScaleFactorChanged { scale_factor });

        let clock = Clock::default();
        // Normal damage path: dynamic updates while the panel is busy, then a
        // final partial update of the accumulated region once it settles.
        let mut damage_path = DamagePath::new(clock.clone());
        let mut presenter = Presenter::new(clock.clone());
        let mut force_full_render = false;
        let mut events: Vec<Event> = Vec::new();

        loop {
            slint::platform::update_timers_and_animations();

            let Some(window) = self.window.borrow().clone() else {
                continue;
            };

            presenter.sample_panel(&mut self.surface.borrow_mut().screen);

            let delay = if full_present_requested() {
                // A timer may have requested a transition earlier in this
                // iteration. Do not block before consuming that request.
                Some(Duration::ZERO)
            } else if window.has_active_animations() {
                None
            } else {
                let now = clock.now();
                [
                    slint::platform::duration_until_next_timer_update(),
                    damage_path.deadline(now),
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

            damage_path.finalize(&mut self.surface.borrow_mut(), &self.policy);

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
                        if self.refresh_panel_height() {
                            force_full_render = true;
                        }
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

                let top = self.surface.borrow().top as i32;
                if let Some(evt) = ink_evt_to_slint(scale_factor, top, evt) {
                    window.dispatch_event(evt);
                }
            }

            // After dispatch, so change handlers queued by input (Slint runs
            // `changed` callbacks here) take effect in this iteration's render
            // instead of after the next wakeup.
            slint::platform::update_timers_and_animations();

            if take_full_present_request() && presenter.visible {
                // Consume after timers/event dispatch but before rendering,
                // so the full present contains the screen that requested it
                // and the normal damage path does not race it.
                new_present |= presenter.request(PresentReason::App);
                window.request_redraw();
            }

            presenter.poll_verification(clock.now());

            if new_present {
                // The full present below covers the whole frame; drop any
                // pending dynamic presentation.
                damage_path.drop_waiting();
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
                let mut surface = self.surface.borrow_mut();
                let width = surface.width;
                let render_started = Instant::now();
                let damage = renderer.render(target_pixels(&mut surface.frame), width);
                if full {
                    renderer.set_repaint_buffer_type(renderer::RepaintBufferType::ReusedBuffer);
                    force_full_render = false;
                }
                let render_elapsed = render_started.elapsed();

                let damage_rect = rect_from_phys(&damage);
                let full_damage = damage_rect.width() >= width as i32
                    && damage_rect.height() >= surface.content_height() as i32;
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

                if damage_rect.is_empty() {
                    // Nothing changed on screen (e.g. a tap on a control
                    // without pressed styles); issue no zero-area update.
                    return;
                }

                damage_path.submit(&mut surface, &self.policy, damage_rect, render_elapsed);
            });

            if presenter.issue_due(
                &mut self.surface.borrow_mut(),
                &self.policy,
                force_full_render,
            ) {
                damage_path.presented();
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

/// Maps an InkView event to Slint. Pointer positions are shifted up by `top`
/// (the system panel's rows); presses on the panel itself are not forwarded,
/// since the firmware handles them.
fn ink_evt_to_slint(scale_factor: f32, top: i32, evt: InkEvent) -> Option<WindowEvent> {
    let position =
        |x: i32, y: i32| slint::PhysicalPosition { x, y: y - top }.to_logical(scale_factor);
    let evt = match evt {
        InkEvent::PointerDown { y, .. } if y < top => return None,
        InkEvent::PointerDown { x, y } => WindowEvent::PointerPressed {
            position: position(x, y),
            button: slint::platform::PointerEventButton::Left,
        },
        InkEvent::PointerMove { x, y } => WindowEvent::PointerMoved {
            position: position(x, y.max(top)),
        },
        InkEvent::PointerUp { x, y } => WindowEvent::PointerReleased {
            position: position(x, y.max(top)),
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
    fn scale_follows_dpi_with_fallback() {
        assert_eq!(scale_from_dpi(300), 3.0);
        assert_eq!(scale_from_dpi(212), 2.12);
        assert_eq!(scale_from_dpi(0), DEFAULT_SCALE_FACTOR);
        assert_eq!(scale_from_dpi(1000), DEFAULT_SCALE_FACTOR);
    }

    #[test]
    fn frame_casts_to_render_target() {
        let mut frame = [RGB24(1, 2, 3), RGB24(4, 5, 6)];
        target_pixels(&mut frame)[1] = Pixel(RGB24(7, 8, 9));
        assert_eq!(frame, [RGB24(1, 2, 3), RGB24(7, 8, 9)]);
    }
}
