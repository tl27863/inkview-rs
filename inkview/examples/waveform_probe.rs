//! Measures how long each InkView update keeps the panel busy.
//!
//! For every named update (`PartialUpdate`, `DynamicUpdateBW`, ...) and, on
//! SDK 6.10+ firmware, every waveform through `UpdateEx`, it redraws a band of
//! stripes, issues the update, and polls `IsUpdateInProcess()` until the panel
//! is idle again. The `UpdateEx` timings next to `GetWaveformTimes()` show
//! whether `UpdateEx`'s low flag bits select a `waveform_type_t`.
//!
//! Results go to stderr and to `$PROBE_LOG` (default
//! `/mnt/ext1/applications/waveform-probe.log`). The app closes itself when
//! done. Like pbAnki, updates are issued from a worker thread while
//! `InkViewMain` runs on the main thread, and each update wakes that loop.

use std::fs::File;
use std::io::Write;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use inkview::device::DeviceInfo;
use inkview::screen::{PixelFormat, Screen, UpdFlags, UpdateKind, Waveform, WhichScreen};
use inkview::{bindings::Inkview, Event};

const DEFAULT_LOG: &str = "/mnt/ext1/applications/waveform-probe.log";
/// Longest wait for an update to start or finish.
const TIMEOUT: Duration = Duration::from_secs(4);
const POLL: Duration = Duration::from_millis(5);

static LOG: Mutex<Option<File>> = Mutex::new(None);

fn log(line: String) {
    eprintln!("[probe] {line}");
    if let Some(file) = LOG.lock().unwrap().as_mut() {
        let _ = writeln!(file, "{line}");
    }
}

/// How long the panel stayed busy after an update was issued.
struct Timing {
    /// Until `IsUpdateInProcess()` first reported busy.
    started_after: Option<Duration>,
    /// Until it reported idle again (from issue).
    idle_after: Option<Duration>,
}

impl std::fmt::Display for Timing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let ms = |d: Option<Duration>| d.map_or("-".into(), |d| d.as_millis().to_string());
        write!(
            f,
            "busy_until={}ms first_busy_after={}ms",
            ms(self.idle_after),
            ms(self.started_after)
        )
    }
}

fn measure<P: PixelFormat>(screen: &mut Screen<'static, P>) -> Timing {
    let issued = Instant::now();
    let mut started_after = None;
    while issued.elapsed() < TIMEOUT {
        if screen.is_updating() {
            started_after = Some(issued.elapsed());
            break;
        }
        std::thread::sleep(POLL);
    }
    let mut idle_after = None;
    if started_after.is_some() {
        while issued.elapsed() < TIMEOUT {
            if !screen.is_updating() {
                idle_after = Some(issued.elapsed());
                break;
            }
            std::thread::sleep(POLL);
        }
    }
    Timing {
        started_after,
        idle_after,
    }
}

/// Fills the band with vertical stripes; `phase` inverts them so every step
/// changes every pixel of the band.
fn draw_band<P: PixelFormat>(screen: &mut Screen<'static, P>, y: usize, h: usize, phase: bool) {
    let w = screen.width();
    let row: Vec<P> = (0..w)
        .map(|x| {
            let black = ((x / 24) % 2 == 0) ^ phase;
            let v = if black { 0 } else { 255 };
            P::from_rgb24(v, v, v)
        })
        .collect();
    for dy in 0..h {
        screen.copy_rect_from(0, y + dy, w, 1, &row, w, (0, y + dy));
    }
}

fn settle<P: PixelFormat>(screen: &mut Screen<'static, P>) {
    let started = Instant::now();
    while screen.is_updating() && started.elapsed() < TIMEOUT {
        std::thread::sleep(POLL);
    }
    std::thread::sleep(Duration::from_millis(300));
}

fn probe<P: PixelFormat>(iv: &'static Inkview, mut screen: Screen<'static, P>) {
    let (w, h) = (screen.width(), screen.height());
    let band_h = h / 6;
    let band_y = h / 2 - band_h / 2;
    log(format!(
        "screen {w}x{h} dpi={} band {w}x{band_h}+0+{band_y}",
        screen.dpi()
    ));

    let times = screen.waveform_times();
    match times {
        Some(t) => log(format!("GetWaveformTimes (ms by waveform id): {t:?}")),
        None => log("GetWaveformTimes unavailable".into()),
    }

    // A clean, fully white screen first.
    let all_white: Vec<P> = vec![P::from_rgb24(255, 255, 255); w];
    for y in 0..h {
        screen.copy_rect_from(0, y, w, 1, &all_white, w, (0, y));
    }
    screen.update(UpdateKind::Full, 0, 0, w as u32, h as u32);
    screen.wake_event_loop();
    settle(&mut screen);

    let mut phase = false;
    for kind in UpdateKind::ALL {
        phase = !phase;
        draw_band(&mut screen, band_y, band_h, phase);
        screen.update(kind, 0, band_y as i32, w as u32, band_h as u32);
        screen.wake_event_loop();
        let timing = measure(&mut screen);
        log(format!("named {kind:<12} {timing}"));
        settle(&mut screen);
    }

    if screen.has_update_ex() {
        for waveform in Waveform::ALL {
            phase = !phase;
            draw_band(&mut screen, band_y, band_h, phase);
            screen.update_ex(
                waveform,
                UpdFlags::NONE,
                0,
                band_y as i32,
                w as u32,
                band_h as u32,
            );
            screen.wake_event_loop();
            let timing = measure(&mut screen);
            let expected = times.map(|t| t[waveform as usize]);
            log(format!(
                "ex:{waveform:<6} (id {}) {timing} GetWaveformTimes={}",
                waveform as u32,
                expected.map_or("-".into(), |ms| format!("{ms}ms"))
            ));
            settle(&mut screen);
        }
    } else {
        log("UpdateEx unavailable (SDK < 6.10 bindings or firmware)".into());
    }

    // A final clean refresh, then exit.
    for y in 0..h {
        screen.copy_rect_from(0, y, w, 1, &all_white, w, (0, y));
    }
    screen.update(UpdateKind::Full, 0, 0, w as u32, h as u32);
    screen.wake_event_loop();
    settle(&mut screen);
    log("done".into());
    unsafe { iv.CloseApp() };
}

fn main() {
    let path = std::env::var("PROBE_LOG").unwrap_or_else(|_| DEFAULT_LOG.into());
    *LOG.lock().unwrap() = File::create(&path).ok();

    let iv: &'static Inkview = Box::leak(Box::new(inkview::load()));
    let mut started = false;
    inkview::iv_main(iv, move |event| {
        if matches!(event, Event::Init) && !started {
            started = true;
            log(format!("device: {}", DeviceInfo::query(iv)));
            std::thread::spawn(move || match WhichScreen::new(iv) {
                WhichScreen::BB8(screen) => probe(iv, screen),
                WhichScreen::RGB24(screen) => probe(iv, screen),
            });
        }
        Some(())
    });
}
