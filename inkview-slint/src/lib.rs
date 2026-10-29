use inkview::event::Key;
use inkview::{screen::Screen, Event};
use rgb::RGB;
use slint::platform::{
    software_renderer::{self as renderer, PhysicalRegion},
    WindowEvent,
};
use std::{
    cell::RefCell,
    rc::Rc,
    sync::mpsc::Receiver,
    time::{Duration, Instant},
};

const SCALE_FACTOR: f32 = 3.0;

/// Damage larger than this is treated as a full UI transition (a card or the
/// loading screen), not an animation. Such regions must not use DynamicUpdate.
const LARGE_DAMAGE_AREA: i32 = 20_000;

/// How long to let InkView settle after Show/Foreground/Repaint before
/// presenting the whole retained frame. The panel can still be updating from a
/// task switch when the first frame is rendered, so an immediate PartialUpdate
/// is discarded. This is deliberately short; the full present below is retried
/// while the panel reports that it is busy.
const SHOW_SETTLE: Duration = Duration::from_millis(300);

#[repr(transparent)]
#[derive(Clone, Copy)]
struct GrayScalePixel(u8);

impl GrayScalePixel {
    fn from_rgb(red: u8, green: u8, blue: u8) -> Self {
        Self(((red as u16 + green as u16 + blue as u16) / 3) as u8)
    }
}

pub struct Backend {
    screen: RefCell<Screen<'static>>,
    evts: Receiver<Event>,
    width: usize,
    height: usize,
    window: RefCell<Option<Rc<renderer::MinimalSoftwareWindow>>>,
    buffer: RefCell<Vec<RGB<u8>>>,
}

impl Backend {
    pub fn new(screen: Screen<'static>, evts: Receiver<Event>) -> Self {
        let width = screen.width();
        let height = screen.height();

        eprintln!("[eink] inkview-slint patched backend (pbanki-eink-refresh)");

        let buffer = vec![Default::default(); width * height];

        Self {
            screen: screen.into(),
            evts,
            width,
            height,
            window: Default::default(),
            buffer: buffer.into(),
        }
    }

    /// Copy the retained frame into the task framebuffer.
    ///
    /// InkView can blank or replace the task framebuffer while another task is
    /// in front. Re-blitting the retained frame before a full-screen update
    /// makes a show/resume independent of whatever currently sits in that
    /// buffer.
    fn blit_all(&self, screen: &mut Screen<'static>) {
        let buffer = self.buffer.borrow();
        for y in 0..self.height {
            for x in 0..self.width {
                let c = buffer[y * self.width + x];
                let c = GrayScalePixel::from_rgb(c.r, c.g, c.b);
                screen.draw(x, y, c.0);
            }
        }
    }
}

fn rect_from_phys(r: PhysicalRegion) -> euclid::Rect<i32, euclid::UnknownUnit> {
    euclid::Rect::new(
        euclid::Point2D::new(r.bounding_box_origin().x, r.bounding_box_origin().y),
        euclid::Size2D::new(
            r.bounding_box_size().width as i32,
            r.bounding_box_size().height as i32,
        ),
    )
}

impl slint::platform::Platform for Backend {
    fn create_window_adapter(
        &self,
    ) -> Result<Rc<dyn slint::platform::WindowAdapter>, slint::PlatformError> {
        let window =
            renderer::MinimalSoftwareWindow::new(renderer::RepaintBufferType::ReusedBuffer);
        self.window.replace(Some(window.clone()));
        Ok(window)
    }

    fn run_event_loop(&self) -> Result<(), slint::PlatformError> {
        // self.window
        //     .borrow()
        //     .as_ref()
        //     .unwrap()
        //     .set_size(slint::PhysicalSize::new(
        //         self.width as u32,
        //         self.height as u32,
        //     ));

        slint::Window::set_size(
            self.window.borrow().as_ref().unwrap().as_ref(),
            slint::PhysicalSize::new(self.width as u32, self.height as u32)
                .to_logical(SCALE_FACTOR),
        );

        self.window
            .borrow()
            .as_ref()
            .unwrap()
            .dispatch_event(WindowEvent::ScaleFactorChanged {
                scale_factor: SCALE_FACTOR,
            });

        // slint::Window::set_size(
        //     self.window.borrow().as_ref().unwrap().as_ref(),
        //     slint::PhysicalSize::new(self.width as u32, self.height as u32).to_logical(SCALE_FACTOR),
        // );

        // bad naming, oops
        let mut fulfill_dynamic_updates_after: Option<Instant> = None;
        let mut dynamic_region_to_redraw: Option<euclid::Rect<i32, euclid::UnknownUnit>> = None;
        let mut accumulated_updates: Option<euclid::Rect<i32, euclid::UnknownUnit>> = None;
        let mut dynamic_finalize_retries: u32 = 0;
        let mut last_draw_at = Instant::now();

        // A full present is delayed after Show/Foreground/Repaint. InkView can
        // still be updating the panel from the task switch when the first
        // frame is rendered; an immediate PartialUpdate is then discarded.
        // Keeping a pending full present and re-blitting the retained frame
        // makes the first frame (and resume) independent of that race.
        let mut full_present_at: Option<Instant> = Some(Instant::now() + SHOW_SETTLE);
        let mut full_present_retries: u32 = 0;
        let mut first_full_render_done = false;
        let mut force_full_render = false;

        loop {
            slint::platform::update_timers_and_animations();

            if let Some(window) = self.window.borrow().clone() {
                let now = Instant::now();
                if full_present_at.is_some_and(|at| now >= at) && !first_full_render_done {
                    // The first present has no complete frame yet. Force one
                    // full render so the re-blit below has valid pixels.
                    force_full_render = true;
                    window.request_redraw();
                }

                let delay = if window.has_active_animations() {
                    None
                } else {
                    [
                        slint::platform::duration_until_next_timer_update(),
                        fulfill_dynamic_updates_after
                            .map(|i| i.saturating_duration_since(Instant::now())),
                        full_present_at.map(|i| i.saturating_duration_since(Instant::now())),
                    ]
                    .into_iter()
                    .flatten()
                    .min()
                    .or(Some(Duration::from_millis(1000)))
                };

                let evt = if let Some(delay) = delay {
                    self.evts.recv_timeout(delay).ok()
                } else if window.has_active_animations() {
                    self.evts.try_recv().ok()
                } else {
                    self.evts.recv().ok()
                };

                if let Some(redraw_region) = dynamic_region_to_redraw {
                    if last_draw_at.elapsed() > Duration::from_millis(200) {
                        let mut screen = self.screen.borrow_mut();

                        if screen.is_updating() {
                            // The panel is still busy. Keep the pending region
                            // instead of dropping the final frame; retry shortly.
                            dynamic_finalize_retries += 1;
                            if dynamic_finalize_retries > 100 {
                                // The panel appears stuck; recover with a full
                                // refresh from the current framebuffer.
                                eprintln!(
                                    "[eink] finalize fallback: FullUpdate region={:?}",
                                    redraw_region
                                );
                                screen.full_update();
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
                            last_draw_at = Instant::now();
                        }
                    }
                }

                slint::platform::update_timers_and_animations();

                if let Some(evt) = evt {
                    match evt {
                        Event::Show | Event::Repaint | Event::Foreground { .. } => {
                            full_present_at = Some(Instant::now() + SHOW_SETTLE);
                            full_present_retries = 0;
                            // Drop any pending dynamic presentation; the full
                            // present below covers the whole frame.
                            dynamic_region_to_redraw = None;
                            accumulated_updates = None;
                            fulfill_dynamic_updates_after = None;
                            dynamic_finalize_retries = 0;
                            window.request_redraw();
                            eprintln!("[eink] show repaint scheduled");
                        }
                        Event::Background { .. } | Event::Hide => {
                            // Do not present into a task that is no longer visible.
                            full_present_at = None;
                            full_present_retries = 0;
                            force_full_render = false;
                        }
                        _ => {}
                    }

                    if let Some(evt) = ink_evt_to_slint(evt) {
                        window.dispatch_event(evt);
                    }
                }

                window.draw_if_needed(|renderer| {
                    let full = force_full_render;
                    if full {
                        renderer.set_repaint_buffer_type(renderer::RepaintBufferType::NewBuffer);
                    }
                    let mut buffer = self.buffer.borrow_mut();
                    let damage = renderer.render(buffer.as_mut_slice(), self.width);
                    if full {
                        renderer.set_repaint_buffer_type(renderer::RepaintBufferType::ReusedBuffer);
                        force_full_render = false;
                        first_full_render_done = true;
                        eprintln!("[eink] forced full frame rendered");
                    }
                    let mut screen = self.screen.borrow_mut();

                    for dy in 0..damage.bounding_box_size().height {
                        for dx in 0..damage.bounding_box_size().width {
                            let x = damage.bounding_box_origin().x + dx as i32;
                            let y = damage.bounding_box_origin().y + dy as i32;
                            let c = buffer[y as usize * self.width + x as usize];

                            let c = GrayScalePixel::from_rgb(c.r, c.g, c.b);

                            screen.draw(x as usize, y as usize, c.0);
                        }
                    }

                    // println!("Drawing to: {:?}", damage);

                    let damage_rect = rect_from_phys(damage.clone());

                    if !first_full_render_done
                        && damage_rect.width() >= self.width as i32
                        && damage_rect.height() >= self.height as i32
                    {
                        first_full_render_done = true;
                        eprintln!("[eink] first full frame rendered");
                    }

                    if full_present_at.is_some() {
                        // The pending full present re-blits and presents the
                        // whole retained frame; skip competing updates here.
                    } else if screen.is_updating() {
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
                            screen.dynamic_update(
                                redraw_region.origin.x,
                                redraw_region.origin.y,
                                redraw_region.width() as u32,
                                redraw_region.height() as u32,
                            );
                            last_draw_at = Instant::now();
                        }

                        if let Some(r) = dynamic_region_to_redraw.as_mut() {
                            *r = r.union(&damage_rect);
                        } else {
                            dynamic_region_to_redraw = Some(damage_rect);
                        }

                        fulfill_dynamic_updates_after =
                            Some(Instant::now() + Duration::from_millis(200));
                    } else {
                        let damage_area = damage_rect.width() * damage_rect.height();
                        if damage_area > LARGE_DAMAGE_AREA {
                            // A large transition can follow a previous update
                            // faster than IsUpdateInProcess() reports it. Wait
                            // so the loading-to-card update is not discarded.
                            screen.wait_for_update_complete();
                        }

                        accumulated_updates = None;
                        screen.partial_update(
                            damage.bounding_box_origin().x,
                            damage.bounding_box_origin().y,
                            damage.bounding_box_size().width,
                            damage.bounding_box_size().height,
                        );
                        last_draw_at = Instant::now();
                    }
                });

                if let Some(at) = full_present_at {
                    if Instant::now() >= at {
                        let mut screen = self.screen.borrow_mut();

                        if screen.is_updating() {
                            full_present_retries += 1;
                            if full_present_retries == 1 {
                                eprintln!("[eink] full repaint waiting for panel");
                            }
                            if full_present_retries > 100 {
                                // The panel appears stuck; present the retained
                                // frame with a flashing full refresh.
                                self.blit_all(&mut *screen);
                                screen.full_update();
                                eprintln!("[eink] full repaint fallback: FullUpdate");
                                full_present_at = None;
                                full_present_retries = 0;
                                dynamic_region_to_redraw = None;
                                accumulated_updates = None;
                                fulfill_dynamic_updates_after = None;
                                last_draw_at = Instant::now();
                            } else {
                                full_present_at = Some(Instant::now() + Duration::from_millis(20));
                            }
                        } else {
                            self.blit_all(&mut *screen);
                            screen.partial_update(0, 0, self.width as u32, self.height as u32);
                            eprintln!("[eink] full repaint: reblit + partial");
                            full_present_at = None;
                            full_present_retries = 0;
                            dynamic_region_to_redraw = None;
                            accumulated_updates = None;
                            fulfill_dynamic_updates_after = None;
                            last_draw_at = Instant::now();
                        }
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

fn ink_evt_to_slint(evt: Event) -> Option<WindowEvent> {
    println!("evt: {:?}", evt);
    let evt = match evt {
        Event::PointerDown { x, y } => WindowEvent::PointerPressed {
            position: slint::PhysicalPosition { x, y }.to_logical(SCALE_FACTOR),
            button: slint::platform::PointerEventButton::Left,
        },
        Event::PointerMove { x, y } => WindowEvent::PointerMoved {
            position: slint::PhysicalPosition { x, y }.to_logical(SCALE_FACTOR),
        },
        Event::PointerUp { x, y } => WindowEvent::PointerReleased {
            position: slint::PhysicalPosition { x, y }.to_logical(SCALE_FACTOR),
            button: slint::platform::PointerEventButton::Left,
        },
        Event::Foreground { .. } => WindowEvent::WindowActiveChanged(true),
        Event::Background { .. } => WindowEvent::WindowActiveChanged(false),
        Event::KeyDown { key } => {
            if let Some(slint_key) = ink_key_to_slint(key) {
                WindowEvent::KeyPressed {
                    text: slint_key.into(),
                }
            } else {
                return None;
            }
        }
        Event::KeyRepeat { key } => {
            if let Some(slint_key) = ink_key_to_slint(key) {
                WindowEvent::KeyPressRepeated {
                    text: slint_key.into(),
                }
            } else {
                return None;
            }
        }
        Event::KeyUp { key } => {
            if let Some(slint_key) = ink_key_to_slint(key) {
                WindowEvent::KeyReleased {
                    text: slint_key.into(),
                }
            } else {
                return None;
            }
        }
        _ => return None,
    };

    Some(evt)
}
