//! Supervision, frame pipeline and HID client against the real helpers and a booted
//! simulator. Runs only with SIMDRIVER_IT=1.
mod support;

use simdriver::__internal::{
    FrameMsg, FramePipeline, HidClient, HidCmd, HidEvent, ShmReader, Supervised, helper_dir,
    parse_frame_line,
};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use support::FrameStats;

/// First output line within this long counts as "helper is up" (startup is slow under load).
const UP_WAIT: Duration = Duration::from_secs(30);

/// This test checks that HidClient forwards a gesture start while displaced and reports
/// `Reclaimed`; how fast sdhid reconnects is sdhid's business (it_sdhid, Task 9's 2 s bar).
/// Measured here: ~0.6 s typically, 2-3 s at times (sdhid's connect retry), so allow 10 s
/// and print the time.
const RECLAIM_WAIT: Duration = Duration::from_secs(10);

fn pid_gone(pid: u32) -> bool {
    unsafe { libc::kill(pid as libc::pid_t, 0) != 0 && *libc::__error() == libc::ESRCH }
}

/// How long until `pid` is gone, polling up to `within`.
fn time_until_gone(pid: u32, within: Duration) -> Option<Duration> {
    let t0 = Instant::now();
    loop {
        if pid_gone(pid) {
            return Some(t0.elapsed());
        }
        if t0.elapsed() >= within {
            return None;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

struct Spawned {
    helper: Supervised,
    lines: Receiver<String>,
    exit: Receiver<Option<i32>>,
}

/// sdframe (explicit --fps/--scale: its built-in defaults are 30/1) or sdhid on `udid`.
fn spawn(name: &str, udid: &str) -> Spawned {
    let args: Vec<String> = match name {
        "sdframe" => vec![
            udid.into(),
            "--fps".into(),
            "60".into(),
            "--scale".into(),
            "2".into(),
        ],
        _ => vec![udid.into()],
    };
    let (ltx, lines) = mpsc::channel();
    let (etx, exit) = mpsc::channel();
    let helper = Supervised::spawn(
        helper_dir(None).join(name),
        args,
        move |l| {
            let _ = ltx.send(l);
        },
        move |c| {
            let _ = etx.send(c);
        },
    )
    .unwrap_or_else(|e| panic!("spawn {name}: {e}"));
    Spawned {
        helper,
        lines,
        exit,
    }
}

/// Wait for the helper's first stdout line (sdframe: its `shm` line; sdhid: `ready`).
fn first_line(name: &str, s: &Spawned) -> (String, Duration) {
    let t0 = Instant::now();
    let l = s.lines.recv_timeout(UP_WAIT).unwrap_or_else(|_| {
        panic!(
            "{name} printed nothing within {UP_WAIT:?} (stderr: {:?})",
            s.helper.last_stderr()
        )
    });
    (l, t0.elapsed())
}

#[test]
fn helpers_exit_on_drop() {
    let Some(udid) = support::booted_udid() else {
        return;
    };
    for name in ["sdframe", "sdhid"] {
        let s = spawn(name, &udid);
        let (line, up) = first_line(name, &s);
        let pid = s.helper.pid();
        let shm = match parse_frame_line(&line) {
            Some(FrameMsg::Shm { name, slots, cap }) => Some((name, slots, cap)),
            _ => None,
        };
        let t0 = Instant::now();
        drop(s.helper);
        let gone = time_until_gone(pid, Duration::from_secs(1));
        eprintln!(
            "{name}: up in {up:?} ({line}); drop returned in {:?}; gone after {gone:?}",
            t0.elapsed()
        );
        assert!(
            gone.is_some(),
            "{name} pid {pid} still alive 1 s after drop"
        );
        assert!(
            s.exit.recv_timeout(Duration::from_secs(1)).is_ok(),
            "{name}: on_exit did not fire"
        );
        if let Some((seg, slots, cap)) = shm {
            assert!(
                ShmReader::open(&seg, slots, cap).is_err(),
                "{seg} still opens after sdframe stopped"
            );
        }
    }
}

#[test]
fn helpers_exit_on_stdin_eof() {
    let Some(udid) = support::booted_udid() else {
        return;
    };
    // Both running at once, so neither can hold the other's stdin open via an inherited fd.
    let frame = spawn("sdframe", &udid);
    let hid = spawn("sdhid", &udid);
    first_line("sdframe", &frame);
    first_line("sdhid", &hid);
    for (name, s) in [("sdframe", &frame), ("sdhid", &hid)] {
        let pid = s.helper.pid();
        s.helper.close_stdin(); // only the fd close: no SIGINT, no SIGKILL
        let gone = time_until_gone(pid, Duration::from_secs(1));
        let code = s.exit.recv_timeout(Duration::from_secs(1));
        eprintln!("{name}: gone {gone:?} after stdin EOF, exit {code:?}");
        assert!(
            gone.is_some(),
            "{name} pid {pid} still alive 1 s after stdin EOF"
        );
        assert_eq!(code, Ok(Some(0)), "{name} must exit 0 on stdin EOF");
    }
}

#[test]
fn it_frame_pipeline_delivers_frames() {
    let Some(udid) = support::booted_udid() else {
        return;
    };
    let _server = support::open_page(&udid, "anim.html");
    std::thread::sleep(Duration::from_secs(2)); // let Safari load and start animating

    let times = Arc::new(Mutex::new(Vec::<Instant>::new()));
    let dims = Arc::new(Mutex::new(None));
    let (t2, d2) = (times.clone(), dims.clone());
    let pipeline = FramePipeline::new(
        move |f| {
            t2.lock().unwrap().push(Instant::now());
            *d2.lock().unwrap() = Some((
                f.width,
                f.height,
                f.source_width,
                f.source_height,
                f.data.len(),
            ));
        },
        |_| {},
    );
    let helper = Supervised::spawn(
        helper_dir(None).join("sdframe"),
        vec![
            udid.clone(),
            "--fps".into(),
            "60".into(),
            "--scale".into(),
            "2".into(),
        ],
        pipeline.sink(),
        |_| {},
    )
    .expect("spawn sdframe");

    let t0 = Instant::now();
    while times.lock().unwrap().is_empty() {
        assert!(
            t0.elapsed() < UP_WAIT,
            "no frame through the pipeline within {UP_WAIT:?}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let first = t0.elapsed();
    std::thread::sleep(Duration::from_secs(1)); // warm-up
    let start = Instant::now();
    std::thread::sleep(Duration::from_secs(5));
    let end = Instant::now();
    drop(helper);
    let stats = FrameStats::over_window(&times.lock().unwrap(), start, end);
    let (w, h, sw, sh, len) = dims.lock().unwrap().expect("dims");
    eprintln!("first frame after {first:?}; {w}x{h} (source {sw}x{sh}); {stats:?}");
    assert!(
        w > 0 && h > 0 && len == (w * h * 4) as usize,
        "bad frame {w}x{h} len {len}"
    );
    assert!(
        w <= sw && h <= sh,
        "payload {w}x{h} larger than source {sw}x{sh}"
    );
    assert!(stats.frames > 0, "no frames in the 5 s window");
}

#[test]
fn it_hid_client_ready_displaced_reclaimed() {
    let Some(udid) = support::booted_udid() else {
        return;
    };
    let (tx, rx) = mpsc::channel();
    let tx = Mutex::new(tx);
    let client = HidClient::spawn(
        helper_dir(None).join("sdhid"),
        vec![udid.clone()],
        move |e| {
            let _ = tx.lock().unwrap().send(e);
        },
    )
    .expect("spawn sdhid");
    let mut seen = Vec::new();
    let mut expect = |want: &str, within: Duration| -> Duration {
        let t0 = Instant::now();
        loop {
            let left = within.saturating_sub(t0.elapsed());
            match rx.recv_timeout(left) {
                Ok(HidEvent::Warn(w)) => seen.push(format!("warn({w})")),
                Ok(HidEvent::Note(n)) => seen.push(format!("note({n})")),
                Ok(e) => {
                    seen.push(format!("{e:?}"));
                    let name = format!("{e:?}");
                    assert!(
                        name.starts_with(want),
                        "expected {want}, got {e:?}; history {seen:?}"
                    );
                    return t0.elapsed();
                }
                Err(_) => panic!("no {want} within {within:?}; history {seen:?}"),
            }
        }
    };
    let ready = expect("Ready", UP_WAIT);
    let (w, h) = client.screen_pt().expect("screen_pt after Ready");
    assert!(w > 100.0 && h > 100.0, "screen {w}x{h}");

    client.send_raw_line(r#"{"t":"_invalidate"}"#);
    let displaced = expect("Displaced", Duration::from_secs(2));
    assert!(client.is_displaced());
    // A gesture start goes through while displaced and reclaims. Left shift press+release:
    // harmless whatever is on screen.
    // sdhid reconnects in ~0.6 s once the cancelled connection has settled; a start sent
    // right after `_invalidate` measured 2.1-2.9 s (sdhid retrying), so wait as it_sdhid does.
    std::thread::sleep(Duration::from_millis(500));
    client.send(HidCmd::Key { code: 225 });
    let reclaimed = expect("Reclaimed", RECLAIM_WAIT);
    assert!(!client.is_displaced());

    let pid = client.pid().unwrap();
    drop(client);
    assert!(pid_gone(pid), "sdhid survived HidClient drop");
    eprintln!(
        "ready after {ready:?} ({w}x{h} pt); displaced after {displaced:?}; reclaimed after {reclaimed:?}"
    );
}
