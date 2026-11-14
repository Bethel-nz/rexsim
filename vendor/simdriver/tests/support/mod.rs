//! Shared helpers for the integration tests that need a booted simulator.
//! Each test binary pulls in only part of this module.
#![allow(dead_code)]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// The simulator to run against, or `None` (and a skip note) unless `SIMDRIVER_IT=1`.
/// `SIMDRIVER_TEST_UDID` picks the device; otherwise the first booted one.
pub fn booted_udid() -> Option<String> {
    if std::env::var("SIMDRIVER_IT").as_deref() != Ok("1") {
        eprintln!("skipping: set SIMDRIVER_IT=1 to run simulator integration tests");
        return None;
    }
    if let Ok(udid) = std::env::var("SIMDRIVER_TEST_UDID")
        && !udid.is_empty()
    {
        return Some(udid);
    }
    let out = Command::new("xcrun")
        .args(["simctl", "list", "devices", "booted", "-j"])
        .output()
        .expect("run xcrun simctl list");
    assert!(out.status.success(), "simctl list failed");
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("simctl json");
    let devices = v["devices"].as_object().expect("devices map");
    for list in devices.values() {
        for d in list.as_array().into_iter().flatten() {
            if d["state"] == "Booted"
                && let Some(udid) = d["udid"].as_str()
            {
                return Some(udid.to_string());
            }
        }
    }
    panic!("SIMDRIVER_IT=1 but no simulator is booted");
}

/// False (with a skip note) while a host of the owner's streams `udid`: no scripted input on
/// a simulator someone is using. Hosts run `sdframe <udid> …` / `sdhid <udid>` (simdriver:
/// Vellum, the ported pip-mini) or `simframe` / `simhid` (the old pip-mini prototype).
/// Call it BEFORE starting the test's own `Driver`, whose helpers would match too.
pub fn input_allowed(udid: &str) -> bool {
    let pattern = format!("/(simframe|simhid|sdframe|sdhid) {udid}");
    let busy = Command::new("pgrep")
        .args(["-f", &pattern])
        .output()
        .is_ok_and(|o| o.status.success());
    if busy {
        eprintln!(
            "skipping: a simdriver/pip-mini host is live on {udid} (no scripted input); use SIMDRIVER_TEST_UDID"
        );
    }
    !busy
}

pub fn fixture_path(name: &str) -> PathBuf {
    // Relative to the simdriver crate, so other crates' tests can share this module.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../simdriver/tests/fixtures")
        .join(name)
}

/// A tiny HTTP server serving one fixture on a free localhost port. Stops on drop.
pub struct Server {
    pub port: u16,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(("127.0.0.1", self.port)); // unblock accept()
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Server {
    /// The page's URL. Names 127.0.0.1, not `localhost`: Safari tries `::1` first, and
    /// std's bind sets SO_REUSEADDR, so our 127.0.0.1 bind succeeds even while another
    /// server (a stray `python -m http.server`) holds `[::]` on the same port — and
    /// `localhost` would load that server's page instead of the fixture.
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}/", self.port)
    }
}

const PREFERRED_PORT: u16 = 47811;

/// Serve `fixture` on a free localhost port and open it in the simulator's Safari.
pub fn open_page(udid: &str, fixture: &str) -> Server {
    let body = std::fs::read(fixture_path(fixture)).expect("read fixture");
    // Prefer one stable port: re-opening the same URL reuses Safari's tab, while every
    // new origin opens another tab and stalls its animation for several seconds.
    let listener = TcpListener::bind(("127.0.0.1", PREFERRED_PORT))
        .or_else(|_| TcpListener::bind(("127.0.0.1", 0)))
        .expect("bind");
    let port = listener.local_addr().unwrap().port();
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = stop.clone();
    let thread = std::thread::spawn(move || {
        for conn in listener.incoming() {
            if stop2.load(Ordering::SeqCst) {
                break;
            }
            let Ok(mut s) = conn else { continue };
            // One thread per connection: Safari opens speculative connections it never
            // writes to, and a serial loop would park its full read timeout behind each.
            let body = body.clone();
            std::thread::spawn(move || {
                let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
                let _ = s.set_write_timeout(Some(Duration::from_secs(2)));
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf);
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = s.write_all(head.as_bytes());
                let _ = s.write_all(&body);
            });
        }
    });
    let url = format!("http://127.0.0.1:{port}/"); // see `Server::url`
    // Once per test process, start from a cold Safari: it keeps every tab this sim ever
    // opened (someone's manual browsing, a previous run's page with a lost touchend),
    // and a warm open can land on restored state
    // instead of the fixture. Killing it per test would re-spawn Safari's whole guest
    // process tree every time — visible as host load.
    static COLD_ONCE: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    let cold = COLD_ONCE.set(()).is_ok();
    if cold {
        let _ = Command::new("xcrun")
            .args(["simctl", "terminate", udid, "com.apple.mobilesafari"])
            .status();
        std::thread::sleep(Duration::from_millis(300));
    }
    let opened = |args: &Vec<String>| {
        Command::new("xcrun")
            .args(args)
            .status()
            .is_ok_and(|st| st.success())
    };
    let launch_with_url = || {
        opened(&vec![
            "simctl".into(),
            "launch".into(),
            udid.into(),
            "com.apple.mobilesafari".into(),
            url.clone(),
        ])
    };
    let st = if cold {
        // After the kill, openurl would relaunch Safari and let it restore its old
        // tabs, sometimes foregrounding one seconds later; launching with the URL
        // argument puts the fixture in front and keeps it there.
        launch_with_url()
    } else {
        // Warm: re-opening the stable-port URL reuses its tab.
        opened(&vec![
            "simctl".into(),
            "openurl".into(),
            udid.into(),
            url.clone(),
        ]) || launch_with_url()
        // (launch is also the fallback when openurl's LaunchServices channel has wedged
        // for good after enough reboots — ETIMEDOUT forever — which launch is immune to.)
    };
    assert!(st, "could not open {url} on {udid}");
    Server {
        port,
        stop,
        thread: Some(thread),
    }
}

/// Delivery statistics for frames that arrived during a measurement window.
#[derive(Debug, Clone, Copy)]
pub struct FrameStats {
    /// Frames that arrived inside the window.
    pub frames: usize,
    /// `frames` over the window's full length, so a stall anywhere (even at the end) counts.
    pub fps: f64,
    pub p50_gap_ms: f64,
    pub p99_gap_ms: f64,
    pub max_gap_ms: f64,
    /// Gaps longer than `SLOW_GAP_MS`, and how many gaps there were in total.
    pub slow_gaps: usize,
    pub gaps: usize,
}

/// Two display periods at 60 Hz: a gap longer than this is a visibly dropped frame.
pub const SLOW_GAP_MS: f64 = 34.0;

impl FrameStats {
    /// Stats over `[start, end]`. Gaps include window start -> first frame and
    /// last frame -> window end, so frames that stop early show up as one long gap.
    /// Times outside the window are ignored.
    pub fn over_window(times: &[Instant], start: Instant, end: Instant) -> FrameStats {
        assert!(end > start, "empty measurement window");
        let inside: Vec<Instant> = times
            .iter()
            .copied()
            .filter(|t| *t >= start && *t <= end)
            .collect();
        // A frame sitting exactly on a window edge is that edge (no zero-length gap).
        let mut edges = Vec::with_capacity(inside.len() + 2);
        if inside.first() != Some(&start) {
            edges.push(start);
        }
        edges.extend(inside.iter().copied());
        if inside.last() != Some(&end) {
            edges.push(end);
        }
        let mut gaps: Vec<f64> = edges
            .windows(2)
            .map(|w| w[1].duration_since(w[0]).as_secs_f64() * 1000.0)
            .collect();
        gaps.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let pct = |p: f64| gaps[(((gaps.len() - 1) as f64) * p).round() as usize];
        FrameStats {
            frames: inside.len(),
            fps: inside.len() as f64 / end.duration_since(start).as_secs_f64(),
            p50_gap_ms: pct(0.50),
            p99_gap_ms: pct(0.99),
            max_gap_ms: gaps[gaps.len() - 1],
            slow_gaps: gaps.iter().filter(|g| **g > SLOW_GAP_MS).count(),
            gaps: gaps.len(),
        }
    }

    /// Stats over the span from the first to the last arrival (no window edges known).
    pub fn from_times(times: &[Instant]) -> FrameStats {
        match (times.first(), times.last()) {
            (Some(&a), Some(&b)) if b > a => FrameStats::over_window(times, a, b),
            _ => FrameStats {
                frames: times.len(),
                fps: 0.0,
                p50_gap_ms: 0.0,
                p99_gap_ms: 0.0,
                max_gap_ms: 0.0,
                slow_gaps: 0,
                gaps: 0,
            },
        }
    }

    /// The delivery bar every live-frame test uses: >= 55 fps, p99 gap <= 34 ms,
    /// at most 1% of gaps over 34 ms, and no single gap over 100 ms.
    pub fn assert_live_60(&self) {
        assert!(self.fps >= 55.0, "fps {:.1} < 55 ({self:?})", self.fps);
        assert!(
            self.p99_gap_ms <= SLOW_GAP_MS,
            "p99 gap {:.1} ms > 34 ({self:?})",
            self.p99_gap_ms
        );
        assert!(
            self.slow_gaps * 100 <= self.gaps,
            "{} of {} gaps > 34 ms (> 1%) ({self:?})",
            self.slow_gaps,
            self.gaps
        );
        assert!(
            self.max_gap_ms <= 100.0,
            "max gap {:.1} ms > 100 ({self:?})",
            self.max_gap_ms
        );
    }
}
