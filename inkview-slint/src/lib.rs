//! Slint platform backend for InkView. The e-ink logic (presents, damage
//! path, update policy, retained frame) is `inkview-eink`; this crate adds
//! the Slint window, its event loop, the render and the input mapping.

pub use inkview_eink::{
    EinkScreen, UPDATES_ENV, UpdateMode, UpdatePolicy, full_present_requested, request_full_present,
};

use inkview::Event as InkEvent;
use inkview::event::Key;
use inkview::screen::PixelFormat;
use inkview_eink::{
    EinkOptions, EinkPresenter, PointerKind, Rect, content_pointer, eink_log, scale_from_dpi,
    wait_events,
};
use slint::platform::{
    EventLoopProxy, WindowEvent,
    software_renderer::{self as renderer, PhysicalRegion, PremultipliedRgbaColor, TargetPixel},
};
use std::{
    cell::RefCell,
    rc::Rc,
    sync::mpsc::{Receiver, Sender},
    time::{Duration, Instant},
};

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

/// Backend settings beyond the screen itself.
#[derive(Clone, Copy, Debug, Default)]
pub struct BackendOptions {
    /// Update mode per class of screen change. `INKVIEW_SLINT_UPDATES`
    /// overrides individual classes on top of this.
    pub policy: UpdatePolicy,
}

pub struct Backend<S: EinkScreen + 'static> {
    eink: RefCell<EinkPresenter<S>>,
    sender: Sender<Event>,
    receiver: Receiver<Event>,
    scale_factor: f32,
    window: RefCell<Option<Rc<renderer::MinimalSoftwareWindow>>>,
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

        let trace_events = std::env::var_os("INKVIEW_SLINT_TRACE_EVENTS").is_some();
        eink_log!(
            "inkview-slint patched backend (pbanki-device-panel, presenter) \
             {width}x{height} {}-byte pixels scale={scale_factor} (dpi={} screen scale={}){}",
            S::Pixel::BYTES,
            screen.dpi(),
            screen.scale(),
            if trace_events { ", tracing events" } else { "" }
        );

        let eink = EinkPresenter::new(
            screen,
            EinkOptions {
                policy: options.policy,
                trace_events,
            },
        );
        Self {
            eink: eink.into(),
            sender,
            receiver,
            scale_factor,
            window: Default::default(),
        }
    }

    /// Sizes the Slint window to the content area.
    fn resize_window(&self, (width, height): (usize, usize)) {
        if let Some(window) = self.window.borrow().as_ref() {
            slint::Window::set_size(
                window.as_ref(),
                slint::PhysicalSize::new(width as u32, height as u32).to_logical(self.scale_factor),
            );
        }
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

    /// One iteration follows the order documented on [`EinkPresenter`].
    fn run_event_loop(&self) -> Result<(), slint::PlatformError> {
        let scale_factor = self.scale_factor;
        let mut eink = self.eink.borrow_mut();
        self.resize_window(eink.content_size());

        self.window
            .borrow()
            .as_ref()
            .unwrap()
            .dispatch_event(WindowEvent::ScaleFactorChanged { scale_factor });

        eink.start();
        let mut events: Vec<Event> = Vec::new();

        loop {
            slint::platform::update_timers_and_animations();

            let Some(window) = self.window.borrow().clone() else {
                continue;
            };

            eink.sample_panel();

            let delay = if full_present_requested() {
                // A timer may have requested a transition earlier in this
                // iteration. Do not block before consuming that request.
                Some(Duration::ZERO)
            } else if window.has_active_animations() {
                None
            } else {
                [
                    slint::platform::duration_until_next_timer_update(),
                    eink.deadline(),
                ]
                .into_iter()
                .flatten()
                .min()
                .or(Some(Duration::from_millis(1000)))
            };

            // Block for the first event, then drain everything already queued
            // so back-to-back Show/Foreground produce a single present.
            events.clear();
            if wait_events(&self.receiver, delay, &mut events).is_err() {
                return Ok(());
            }

            eink.before_dispatch();

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
                let lifecycle = eink.on_event(&evt);
                if lifecycle.resized {
                    self.resize_window(eink.content_size());
                }
                if lifecycle.redraw {
                    window.request_redraw();
                }

                if let Some(evt) = ink_evt_to_slint(scale_factor, eink.top() as i32, evt) {
                    window.dispatch_event(evt);
                }
            }

            // After dispatch, so change handlers queued by input (Slint runs
            // `changed` callbacks here) take effect in this iteration's render
            // instead of after the next wakeup.
            slint::platform::update_timers_and_animations();

            if eink.after_dispatch() {
                window.request_redraw();
            }

            window.draw_if_needed(|renderer| {
                let full = eink.full_render_needed();
                if full {
                    renderer.set_repaint_buffer_type(renderer::RepaintBufferType::NewBuffer);
                }
                let width = eink.width();
                let render_started = Instant::now();
                let damage = renderer.render(target_pixels(eink.frame_mut()), width);
                if full {
                    renderer.set_repaint_buffer_type(renderer::RepaintBufferType::ReusedBuffer);
                }
                let render_elapsed = render_started.elapsed();
                eink.submit_frame(rect_from_phys(&damage), render_elapsed, full);
            });

            eink.after_render();
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
    let position = |kind, x, y| {
        let (x, y) = content_pointer(top, kind, x, y)?;
        Some(slint::PhysicalPosition { x, y }.to_logical(scale_factor))
    };
    let evt = match evt {
        InkEvent::PointerDown { x, y } => WindowEvent::PointerPressed {
            position: position(PointerKind::Down, x, y)?,
            button: slint::platform::PointerEventButton::Left,
        },
        InkEvent::PointerMove { x, y } => WindowEvent::PointerMoved {
            position: position(PointerKind::Move, x, y)?,
        },
        InkEvent::PointerUp { x, y } => WindowEvent::PointerReleased {
            position: position(PointerKind::Up, x, y)?,
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
    fn frame_casts_to_render_target() {
        let mut frame = [RGB24(1, 2, 3), RGB24(4, 5, 6)];
        target_pixels(&mut frame)[1] = Pixel(RGB24(7, 8, 9));
        assert_eq!(frame, [RGB24(1, 2, 3), RGB24(7, 8, 9)]);
    }
}
