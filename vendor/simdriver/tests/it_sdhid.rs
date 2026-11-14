//! sdhid against a real booted simulator. Runs only with SIMDRIVER_IT=1.
//!
//! On current simulator runtimes dtuhidd tolerates several clients, so displacement is
//! provoked deterministically with sdhid's test-only `{"t":"_invalidate"}` command, which
//! runs the same code path as XPC_ERROR_CONNECTION_INVALID. Unexpected messages fail with
//! the full history instead of hanging.
mod support;

use simdriver::__internal::{
    Edge, FrameMsg, HidCmd, HidMsg, Phase, ShmReader, parse_frame_line, parse_hid_line,
};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

struct Hid {
    name: &'static str,
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<HidMsg>,
    seen: Vec<String>,
}

impl Drop for Hid {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Hid {
    fn spawn(name: &'static str, udid: &str) -> Hid {
        let bin = simdriver::baked_helper_dir().join("sdhid");
        let mut child = Command::new(&bin)
            .arg(udid)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap_or_else(|e| panic!("spawn {}: {e}", bin.display()));
        let stdin = child.stdin.take().unwrap();
        let out = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                match parse_hid_line(&line) {
                    Some(m) => {
                        if tx.send(m).is_err() {
                            break;
                        }
                    }
                    None => eprintln!("unparsed sdhid line: {line}"),
                }
            }
        });
        Hid {
            name,
            child,
            stdin,
            rx,
            seen: Vec::new(),
        }
    }

    fn send(&mut self, cmd: HidCmd) {
        self.send_line(&cmd.to_line());
    }

    fn send_line(&mut self, line: &str) {
        writeln!(self.stdin, "{line}").expect("write to sdhid");
        self.stdin.flush().expect("flush sdhid stdin");
    }

    /// Next message that is not a warning, within `within`. `None` on timeout.
    fn next(&mut self, within: Duration) -> Option<HidMsg> {
        let deadline = Instant::now() + within;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.rx.recv_timeout(left) {
                Ok(HidMsg::Warn(w)) => self.seen.push(format!("warn({w})")),
                Ok(m) => {
                    self.seen.push(format!("{m:?}"));
                    return Some(m);
                }
                Err(RecvTimeoutError::Timeout) => return None,
                Err(RecvTimeoutError::Disconnected) => {
                    self.seen.push("<stdout closed>".into());
                    return None;
                }
            }
        }
    }

    /// Require the next non-warning message to be `want` within `within`.
    fn expect(&mut self, want: HidMsg, within: Duration) {
        let got = self.next(within);
        assert!(
            got.as_ref() == Some(&want),
            "{}: expected {want:?} within {within:?}, got {got:?}; history: {:?}",
            self.name,
            self.seen
        );
    }

    /// Require that no non-warning message arrives for `quiet`.
    fn expect_silence(&mut self, quiet: Duration, what: &str) {
        let got = self.next(quiet);
        assert!(
            got.is_none(),
            "{}: {what}: unexpected {got:?}; history: {:?}",
            self.name,
            self.seen
        );
    }
}

const READY_WAIT: Duration = Duration::from_secs(30);

fn touch(phase: Phase, x: f64, y: f64) -> HidCmd {
    HidCmd::Touch {
        phase,
        x,
        y,
        edge: Edge::None,
    }
}

/// sdframe, reduced to "how bright is the centre pixel right now".
struct Frames {
    child: Child,
    centre: Arc<Mutex<Option<u8>>>,
}

impl Drop for Frames {
    fn drop(&mut self) {
        drop(self.child.stdin.take());
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Frames {
    fn spawn(udid: &str) -> Frames {
        let bin = simdriver::baked_helper_dir().join("sdframe");
        let mut child = Command::new(&bin)
            .args([udid, "--fps", "30", "--scale", "4"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap_or_else(|e| panic!("spawn {}: {e}", bin.display()));
        let out = child.stdout.take().unwrap();
        let centre = Arc::new(Mutex::new(None));
        let shared = centre.clone();
        std::thread::spawn(move || {
            let mut reader: Option<ShmReader> = None;
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                match parse_frame_line(&line) {
                    Some(FrameMsg::Shm { name, slots, cap }) => {
                        reader =
                            Some(ShmReader::open(&name, slots, cap).expect("open announced shm"));
                    }
                    Some(FrameMsg::Frame { slot, seq }) => {
                        let Some(f) = reader.as_ref().and_then(|r| r.read(slot, seq)) else {
                            continue;
                        };
                        let o = ((f.height / 2 * f.width + f.width / 2) * 4) as usize;
                        let px = &f.data[o..o + 3]; // BGR
                        let lum = ((px[0] as u32 + px[1] as u32 + px[2] as u32) / 3) as u8;
                        *shared.lock().unwrap() = Some(lum);
                    }
                    _ => {}
                }
            }
        });
        Frames { child, centre }
    }

    fn lum(&self) -> Option<u8> {
        *self.centre.lock().unwrap()
    }

    /// Wait until the centre luminance satisfies `pred`; returns the last value seen on timeout.
    fn wait(&self, within: Duration, pred: impl Fn(u8) -> bool) -> Result<Duration, Option<u8>> {
        let start = Instant::now();
        loop {
            if let Some(l) = self.lum()
                && pred(l)
            {
                return Ok(start.elapsed());
            }
            if start.elapsed() >= within {
                return Err(self.lum());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

fn is_black(l: u8) -> bool {
    l < 40
}
fn is_white(l: u8) -> bool {
    l > 215
}

#[test]
fn it_sdhid_steps_aside_and_reclaims() {
    let Some(udid) = support::booted_udid() else {
        return;
    };
    let _server = support::open_page(&udid, "tap.html");

    let mut a = Hid::spawn("A", &udid);
    let ready = a.next(READY_WAIT);
    let Some(HidMsg::Ready { w, h }) = ready else {
        panic!("A: expected Ready, got {ready:?}; history: {:?}", a.seen)
    };
    let (cx, cy) = (w / 2.0, h / 2.0);

    // The page must be up (black) before anything is judged from the screen.
    let frames = Frames::spawn(&udid);
    if let Err(last) = frames.wait(Duration::from_secs(20), is_black) {
        panic!("tap.html never showed a black centre within 20 s (last luminance {last:?})");
    }

    // Displaced: reported once, no reconnect.
    a.send_line(r#"{"t":"_invalidate"}"#);
    a.expect(HidMsg::Displaced, Duration::from_secs(2));

    // A non-start command is dropped: no reclaim, nothing on screen.
    a.send(touch(Phase::Move, cx, cy));
    a.expect_silence(
        Duration::from_millis(500),
        "touch move while displaced must not reclaim",
    );
    assert!(
        is_black(frames.lum().unwrap()),
        "dropped move must not touch the screen"
    );

    // A gesture start reclaims, then lands.
    let t0 = Instant::now();
    a.send(touch(Phase::Down, cx, cy));
    a.expect(HidMsg::Reclaimed, Duration::from_secs(2));
    let reclaimed_in = t0.elapsed();
    let white_in = frames
        .wait(Duration::from_secs(3), is_white)
        .unwrap_or_else(|last| panic!("touch down after reclaim never turned the page white (last {last:?}); history: {:?}", a.seen));

    a.send(touch(Phase::Up, cx, cy));
    let black_in = frames
        .wait(Duration::from_secs(3), is_black)
        .unwrap_or_else(|last| {
            panic!(
                "touch up never turned the page black (last {last:?}); history: {:?}",
                a.seen
            )
        });
    a.expect_silence(
        Duration::from_millis(300),
        "A must stay quiet after reclaiming",
    );
    eprintln!(
        "reclaimed in {reclaimed_in:?}; white after {white_in:?}; black after up {black_in:?}"
    );
}
