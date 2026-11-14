//! The public `Driver` against a real booted simulator. Runs only with SIMDRIVER_IT=1.
//!
//! Tests that inject input skip themselves while the owner's pip-mini streams the same
//! device (see `support::input_allowed`); point them at a spare simulator with
//! SIMDRIVER_TEST_UDID. `it_survives_sim_reboot` also needs SIMDRIVER_IT_REBOOT=1: it shuts
//! the test simulator down and boots it again (leaving it booted).
mod support;

use simdriver::{DeviceSelector, Driver, Error, Event, Modifiers, Options, Phase, State};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use support::FrameStats;

/// Starting -> Live: helpers up and sdhid connected (dtuhidd can take a while under load).
const LIVE_WAIT: Duration = Duration::from_secs(40);

#[derive(Default)]
struct Log {
    frames: Vec<Instant>,
    states: Vec<(Instant, State)>,
    notes: Vec<String>,
    /// Centre-pixel luminance of the latest frame.
    centre: Option<u8>,
    /// Some(true) once a frame showed tap.html's white bar (the page really loaded);
    /// Safari's black loading/error canvas has no full-width white bar.
    loaded: Option<bool>,
    /// Events delivered after `State::Stopped` (must stay 0).
    after_stopped: usize,
}

/// Latched scan: does this frame contain tap.html's bar — a white band a few rows tall
/// with the page's black above and below it? Safari's black loading/error canvas has no
/// white band, and an all-white page (Safari start page, a stale directory listing) has
/// no dark bounds, so neither can satisfy it. Runs only until the first positive per
/// Rec, so the 60 fps path stays cheap.
fn frame_shows_bar(f: &simdriver::Frame) -> bool {
    let (w, h) = (f.width as usize, f.height as usize);
    let lum = |i: usize| {
        let o = i * 4;
        (f.data[o] as u32 + f.data[o + 1] as u32 + f.data[o + 2] as u32) / 3
    };
    let row_is = |row: usize, white: bool| {
        let samples = (w / 20).max(8);
        (0..samples)
            .map(|s| lum(row * w + s * (w - 1) / samples))
            .filter(|&l| if white { l > 215 } else { l < 60 })
            .count()
            * 20
            >= samples * 19 // ≥ 95% of the row
    };
    let mut white = 0;
    for row in (100..h.saturating_sub(100)).step_by(2) {
        if row_is(row, true) {
            white += 1;
            if white >= 3
                && row >= 8 + 100
                && row + 8 < h
                && row_is(row - 8, false)
                && row_is(row + 8, false)
            {
                return true;
            }
        } else {
            white = 0;
        }
    }
    false
}

/// Records everything a Driver reports.
#[derive(Clone, Default)]
struct Rec(Arc<Mutex<Log>>);

impl Rec {
    fn on_event(&self) -> impl Fn(Event) + Send + Sync + 'static {
        let log = self.0.clone();
        move |ev| {
            let mut l = log.lock().unwrap();
            if l.states.last().is_some_and(|(_, s)| *s == State::Stopped) {
                l.after_stopped += 1;
            }
            match ev {
                Event::Frame(f) => {
                    l.frames.push(Instant::now());
                    if l.loaded != Some(true) {
                        l.loaded = Some(frame_shows_bar(&f));
                    }
                    let o = ((f.height / 2 * f.width + f.width / 2) * 4) as usize;
                    let px = &f.data[o..o + 3]; // BGR
                    l.centre = Some(((px[0] as u32 + px[1] as u32 + px[2] as u32) / 3) as u8);
                }
                Event::State(s) => l.states.push((Instant::now(), s)),
                Event::Note(n) => l.notes.push(n),
            }
        }
    }

    fn states(&self) -> Vec<State> {
        self.0
            .lock()
            .unwrap()
            .states
            .iter()
            .map(|(_, s)| *s)
            .collect()
    }

    fn frames(&self) -> usize {
        self.0.lock().unwrap().frames.len()
    }

    fn centre(&self) -> Option<u8> {
        self.0.lock().unwrap().centre
    }

    /// Wait until a frame proved tap.html really loaded (its white bar). Safari's own
    /// black loading/error canvas satisfies a bare "black centre" check, so the tests
    /// require this before trusting any tap state.
    fn wait_loaded(&self, within: Duration) {
        let t0 = Instant::now();
        loop {
            if self.loaded() {
                return;
            }
            assert!(
                t0.elapsed() < within,
                "tap.html never loaded (no bar) within {within:?}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn loaded(&self) -> bool {
        self.0.lock().unwrap().loaded == Some(true)
    }

    /// Wait for state `want` reported at or after index `from` of the state log.
    fn wait_state(&self, from: usize, want: State, within: Duration) -> Duration {
        let t0 = Instant::now();
        loop {
            {
                let l = self.0.lock().unwrap();
                if l.states.iter().skip(from).any(|(_, s)| *s == want) {
                    return t0.elapsed();
                }
                if t0.elapsed() >= within {
                    let tail: Vec<&String> = l.notes.iter().rev().take(8).collect();
                    panic!(
                        "no {want:?} within {within:?}; states {:?}; last notes {tail:?}",
                        self.states_of(&l)
                    );
                }
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn states_of(&self, l: &Log) -> Vec<State> {
        l.states.iter().map(|(_, s)| *s).collect()
    }

    /// Wait until the page animates: at least 30 frames in the last second.
    fn wait_streaming(&self, within: Duration) {
        self.wait_streaming_since(Instant::now() - Duration::from_secs(1), within);
    }

    /// Like `wait_streaming`, counting only frames that arrived at or after `since`.
    fn wait_streaming_since(&self, since: Instant, within: Duration) {
        let t0 = Instant::now();
        loop {
            let recent = {
                let l = self.0.lock().unwrap();
                let cut = (Instant::now() - Duration::from_secs(1)).max(since);
                l.frames.iter().rev().take_while(|t| **t >= cut).count()
            };
            if recent >= 30 {
                return;
            }
            assert!(
                t0.elapsed() < within,
                "page not animating after {within:?} ({recent} frames in the last 1 s)"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Wait until the centre luminance satisfies `pred`.
    fn wait_centre(
        &self,
        within: Duration,
        pred: impl Fn(u8) -> bool,
    ) -> Result<Duration, Option<u8>> {
        let t0 = Instant::now();
        loop {
            if let Some(l) = self.centre()
                && pred(l)
            {
                return Ok(t0.elapsed());
            }
            if t0.elapsed() >= within {
                return Err(self.centre());
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

fn opts(udid: &str) -> Options {
    Options {
        device: DeviceSelector::Udid(udid.to_string()),
        ..Options::default()
    }
}

fn start(udid: &str, rec: &Rec) -> Driver {
    Driver::start(opts(udid), rec.on_event()).unwrap_or_else(|e| panic!("Driver::start: {e}"))
}

fn no_mods() -> Modifiers {
    Modifiers::default()
}

fn pid_gone(pid: u32) -> bool {
    unsafe { libc::kill(pid as libc::pid_t, 0) != 0 && *libc::__error() == libc::ESRCH }
}

fn is_black(l: u8) -> bool {
    l < 40
}
fn is_white(l: u8) -> bool {
    l > 215
}

#[test]
fn defaults() {
    let o = Options::default();
    assert_eq!(o.device, DeviceSelector::FirstBootedOrIphone);
    assert_eq!((o.fps, o.scale, o.helper_dir), (60, 2, None));
}

#[test]
fn not_booted_is_an_error() {
    if std::env::var("SIMDRIVER_IT").as_deref() != Ok("1") {
        eprintln!("skipping: set SIMDRIVER_IT=1");
        return;
    }
    let devices = simdriver::list_devices().expect("list devices");
    let Some(off) = devices.iter().find(|d| d.state == "Shutdown") else {
        eprintln!("skipping: no shut-down simulator to try");
        return;
    };
    let rec = Rec::default();
    match Driver::start(opts(&off.udid), rec.on_event()) {
        Err(Error::NotBooted(u)) => assert_eq!(u, off.udid),
        Err(e) => panic!("expected NotBooted, got {e}"),
        Ok(_) => panic!("started on a shut-down simulator"),
    }
    assert_eq!(rec.frames(), 0);
    assert!(matches!(
        Driver::start(opts("00000000-0000-0000-0000-000000000000"), |_| {}),
        Err(Error::NoDevice)
    ));
}

#[test]
fn it_streams_60fps() {
    let Some(udid) = support::booted_udid() else {
        return;
    };
    let _server = support::open_page(&udid, "anim.html");
    let rec = Rec::default();
    let d = start(&udid, &rec);
    assert_eq!(d.udid(), udid);
    rec.wait_state(0, State::Live, LIVE_WAIT);
    rec.wait_streaming(Duration::from_secs(20));
    std::thread::sleep(Duration::from_secs(1)); // let Safari settle on the page
    let start = Instant::now();
    std::thread::sleep(Duration::from_secs(5));
    let end = Instant::now();
    let stats = d.stats();
    d.stop();
    let fs = FrameStats::over_window(&rec.0.lock().unwrap().frames, start, end);
    eprintln!("{fs:?}; driver stats {stats:?}");
    fs.assert_live_60();
    assert!(
        stats.fps >= 50.0,
        "Stats.fps {} disagrees with the delivered rate",
        stats.fps
    );
    assert_eq!(rec.states().last(), Some(&State::Stopped));
}

#[test]
fn second_driver_replaces_first() {
    let Some(udid) = support::booted_udid() else {
        return;
    };
    let (ra, rb) = (Rec::default(), Rec::default());
    let a = start(&udid, &ra);
    let pids = a.__helper_pids();
    assert_eq!(pids.len(), 2, "sdframe + sdhid pids, got {pids:?}");
    let b = start(&udid, &rb);
    assert_eq!(a.state(), State::Stopped);
    assert_eq!(
        ra.states().last(),
        Some(&State::Stopped),
        "first driver's last event must be Stopped"
    );
    for pid in &pids {
        assert!(
            pid_gone(*pid),
            "first driver's helper {pid} survived the replacement"
        );
    }
    rb.wait_state(0, State::Live, LIVE_WAIT);
    assert!(rb.frames() > 0);
    let a_events = ra.0.lock().unwrap().states.len();
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        ra.0.lock().unwrap().states.len(),
        a_events,
        "events after Stopped"
    );
    assert_eq!(
        ra.0.lock().unwrap().after_stopped,
        0,
        "first driver reported after Stopped"
    );
    a.stop(); // idempotent, and must not touch the new driver
    assert_eq!(b.state(), State::Live);
    let b_pids = b.__helper_pids();
    drop(b);
    for pid in b_pids {
        assert!(pid_gone(pid), "helper {pid} survived drop");
    }
    assert_eq!(rb.states().last(), Some(&State::Stopped));
}

#[test]
fn it_tap_latency() {
    let Some(udid) = support::booted_udid() else {
        return;
    };
    if !support::input_allowed(&udid) {
        return;
    }
    let _server = support::open_page(&udid, "tap.html");
    let rec = Rec::default();
    let d = start(&udid, &rec);
    rec.wait_state(0, State::Live, LIVE_WAIT);
    rec.wait_loaded(Duration::from_secs(20));
    if let Err(last) = rec.wait_centre(Duration::from_secs(20), is_black) {
        panic!("tap.html never showed a black centre (last luminance {last:?})");
    }
    let (mut ms, mut seen_ms) = (Vec::new(), Vec::new());
    for i in 0..10 {
        let t0 = Instant::now();
        d.pointer(Phase::Down, 0.5, 0.5, no_mods());
        let r = loop {
            if let Some(r) = d.stats().last_response_ms {
                break r;
            }
            assert!(
                t0.elapsed() < Duration::from_secs(2),
                "tap {i}: no frame after the down"
            );
            std::thread::sleep(Duration::from_millis(2));
        };
        rec.wait_centre(Duration::from_secs(2), is_white)
            .unwrap_or_else(|last| panic!("tap {i}: page never turned white (last {last:?})"));
        seen_ms.push(t0.elapsed().as_secs_f64() * 1000.0);
        d.pointer(Phase::Up, 0.5, 0.5, no_mods());
        rec.wait_centre(Duration::from_secs(2), is_black)
            .unwrap_or_else(|last| {
                panic!("tap {i}: page never turned black after up (last {last:?})")
            });
        ms.push(r);
        std::thread::sleep(Duration::from_millis(150));
    }
    ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let median = (ms[4] + ms[5]) / 2.0;
    seen_ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
    eprintln!(
        "tap -> first frame (ms): {ms:?}; median {median:.1}; tap -> page white (ms, 5 ms polling): median {:.1}, max {:.1}",
        (seen_ms[4] + seen_ms[5]) / 2.0,
        seen_ms[9]
    );
    assert!(median < 100.0, "median tap latency {median:.1} ms >= 100");
}

#[test]
fn it_displaced_then_reclaimed() {
    let Some(udid) = support::booted_udid() else {
        return;
    };
    if !support::input_allowed(&udid) {
        return;
    }
    let _server = support::open_page(&udid, "anim.html");
    let rec = Rec::default();
    let d = start(&udid, &rec);
    rec.wait_state(0, State::Live, LIVE_WAIT);
    rec.wait_streaming(Duration::from_secs(20));
    let from = rec.states().len();
    d.__invalidate_input_for_test();
    let displaced = rec.wait_state(from, State::InputDisplaced, Duration::from_secs(2));
    assert_eq!(d.state(), State::InputDisplaced);
    // Frames must keep streaming while displaced. Safari's animation itself stalls for a
    // few hundred ms at a time after a cold launch on a loaded host (with input untouched
    // too), so one fixed 1 s window flakes: ask for a streaming second within a few.
    let t_streaming = Instant::now();
    rec.wait_streaming_since(t_streaming, Duration::from_secs(5));
    let streaming = t_streaming.elapsed();
    assert_eq!(
        d.state(),
        State::InputDisplaced,
        "reclaimed before any input"
    );

    let from = rec.states().len();
    d.pointer(Phase::Down, 0.5, 0.5, no_mods());
    let reclaimed = rec.wait_state(from, State::Live, Duration::from_secs(2));
    d.pointer(Phase::Up, 0.5, 0.5, no_mods());
    assert_eq!(d.state(), State::Live);
    eprintln!(
        "displaced after {displaced:?}; streaming while displaced within {streaming:?}; live again {reclaimed:?} after the down"
    );
}

#[test]
fn it_survives_sim_reboot() {
    let Some(udid) = support::booted_udid() else {
        return;
    };
    if std::env::var("SIMDRIVER_IT_REBOOT").as_deref() != Ok("1") {
        eprintln!("skipping: set SIMDRIVER_IT_REBOOT=1 to shut down and reboot the test simulator");
        return;
    }
    if !support::input_allowed(&udid) {
        return;
    }
    let rec = Rec::default();
    let d = start(&udid, &rec);
    rec.wait_state(0, State::Live, LIVE_WAIT);

    let from = rec.states().len();
    let t_off = Instant::now();
    let pids_before = d.__helper_pids();
    let down = simdriver::shutdown(&udid);
    // Boot again whatever happens next: this is someone's dev simulator.
    struct Reboot<'a>(&'a str);
    impl Drop for Reboot<'_> {
        fn drop(&mut self) {
            let _ = simdriver::boot(self.0);
        }
    }
    let _reboot = Reboot(&udid);
    down.expect("shutdown");
    let restarting = rec.wait_state(from, State::Restarting, Duration::from_secs(10));
    let frames_at_restart = rec.frames();
    simdriver::boot(&udid).expect("boot");
    let booted = t_off.elapsed();
    // Live may already be back before `boot` returns (the device reports Booted before
    // bootstatus finishes); either way within 30 s of the boot.
    let live = rec.wait_state(from, State::Live, Duration::from_secs(30));
    assert!(
        rec.frames() > frames_at_restart,
        "no frames after the reboot"
    );
    let pids_after = d.__helper_pids();
    eprintln!("pids before shutdown {pids_before:?} -> after live {pids_after:?}");

    // A touch after the reboot is delivered (reclaiming first if input was displaced).
    let _server = support::open_page(&udid, "tap.html");
    // First load after a reboot is where Safari hiccups; its error canvas is black
    // like the page, so require the bar, and reload once before failing.
    let t0 = Instant::now();
    while !rec.loaded() && t0.elapsed() < Duration::from_secs(25) {
        std::thread::sleep(Duration::from_millis(10));
    }
    if !rec.loaded() {
        let _ = Command::new("xcrun")
            .args([
                "simctl",
                "launch",
                &udid,
                "com.apple.mobilesafari",
                &_server.url(),
            ])
            .status();
        rec.wait_loaded(Duration::from_secs(15));
    }
    if let Err(last) = rec.wait_centre(Duration::from_secs(30), is_black) {
        panic!("tap.html never showed a black centre after the reboot (last {last:?})");
    }
    let from = rec.states().len();
    d.pointer(Phase::Down, 0.5, 0.5, no_mods());
    if rec
        .states()
        .iter()
        .skip(from)
        .any(|s| *s == State::InputDisplaced)
        || d.state() == State::InputDisplaced
    {
        rec.wait_state(from, State::Live, Duration::from_secs(2));
    }
    let white = rec.wait_centre(Duration::from_secs(3), is_white);
    d.pointer(Phase::Up, 0.5, 0.5, no_mods());
    let white = white.unwrap_or_else(|last| {
        let notes = rec
            .0
            .lock()
            .unwrap()
            .notes
            .iter()
            .rev()
            .take(10)
            .cloned()
            .collect::<Vec<_>>();
        panic!(
            "touch after reboot never landed (last {last:?}); states {:?}; notes {notes:?}",
            rec.states()
        )
    });
    eprintln!(
        "Restarting {restarting:?} after shutdown; booted after {booted:?}; Live {live:?} after Restarting; touch landed in {white:?}; states {:?}",
        rec.states()
    );
}

/// The accessibility path end to end: find Settings' "General" row by label, tap its
/// centre through the HID path, and see the General page in the next snapshots.
#[test]
fn it_accessibility_finds_and_taps() {
    let Some(udid) = support::booted_udid() else {
        return;
    };
    if !support::input_allowed(&udid) {
        return;
    }
    let _ = Command::new("xcrun")
        .args(["simctl", "terminate", &udid, "com.apple.Preferences"])
        .status();
    let ok = Command::new("xcrun")
        .args(["simctl", "launch", &udid, "com.apple.Preferences"])
        .status();
    assert!(ok.is_ok_and(|s| s.success()), "could not launch Settings");
    let rec = Rec::default();
    let d = start(&udid, &rec);
    rec.wait_state(0, State::Live, LIVE_WAIT);
    let labelled = |root: &simdriver::AxNode, role: &str, label: &str| {
        root.descendants()
            .into_iter()
            .find(|n| n.role == role && n.label.as_deref() == Some(label))
            .cloned()
    };
    let t0 = Instant::now();
    let general = loop {
        let root = d
            .accessibility()
            .unwrap_or_else(|e| panic!("snapshot: {e}"));
        if let Some(n) = labelled(&root, "AXButton", "General") {
            break n;
        }
        assert!(
            t0.elapsed() < Duration::from_secs(20),
            "Settings never showed a General row"
        );
        std::thread::sleep(Duration::from_millis(200));
    };
    let mut warm = Vec::new();
    for _ in 0..5 {
        let t = Instant::now();
        d.accessibility().unwrap();
        warm.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    warm.sort_by(|a, b| a.partial_cmp(b).unwrap());
    eprintln!("warm snapshots (ms): {warm:?}");
    assert!(warm[2] < 500.0, "median warm snapshot {:.1} ms", warm[2]);

    let (sw, sh) = d.screen_points().unwrap();
    let (cx, cy) = general.center();
    d.tap(cx / sw, cy / sh);
    let t0 = Instant::now();
    loop {
        let root = d.accessibility().unwrap();
        if labelled(&root, "AXButton", "About").is_some() {
            break;
        }
        assert!(
            t0.elapsed() < Duration::from_secs(10),
            "the General page never appeared"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    d.stop();
}
