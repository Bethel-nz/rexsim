//! rexsim: a live iOS Simulator inside a Rex pane.
//!
//! Run `rexsim` from a Rex terminal and it opens a pane beside you that shows the
//! simulator's screen and forwards your mouse and keys to it. Agents (or you) can keep
//! driving the simulator with the `simdriver` CLI at the same time; the pane just watches.
//!
//! How it draws: simdriver streams the simulator's framebuffer as BGRA frames. Each frame
//! goes to the terminal with the Kitty graphics protocol through POSIX shared memory
//! (`t=s`): we write RGBA into a fresh segment and send only its name; the terminal maps it,
//! reads it and unlinks it. Frames alternate between two image ids, the old one deleted in
//! the same synchronized update, so the picture never blanks between frames. One frame is
//! in flight at a time: the next goes once the terminal answers the last, newest wins.
//!
//! Input uses SGR mouse reporting in pixel mode (1016), so a click lands on an exact point
//! of the device screen rather than a whole cell. The pane reports its state to Rex with the
//! Program Status Protocol (OSC 7501), so the tab shows when it is connecting or stopped.
//!
//! Keys: printable text, Enter, Backspace, Tab, Escape and arrows go to the simulator.
//! Ctrl-H presses Home. Ctrl-C quits.
use simdriver::{Button, DeviceSelector, Driver, Event, Frame, Modifiers, Options, Phase, ScrollPhase, State};
use std::collections::VecDeque;
use std::ffi::CString;
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

// ---------- log ----------

static T0: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
static LOG: std::sync::OnceLock<Option<Mutex<std::fs::File>>> = std::sync::OnceLock::new();
/// `--log <file>`, or `$REXSIM_LOG`. Passed on as a flag, because a new Rex pane doesn't
/// inherit this process's environment.
static LOG_PATH: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();

/// One timestamped line to the frame log, when one was asked for.
fn log(msg: impl AsRef<str>) {
    if let Some(Some(f)) = LOG.get() {
        let ms = T0.get().map_or(0.0, |t| t.elapsed().as_secs_f64() * 1000.0);
        let _ = writeln!(f.lock().unwrap(), "{ms:9.1} {}", msg.as_ref());
    }
}

// ---------- program status (OSC 7501) ----------

/// Rex shows this on the tab and in the session picker.
fn status_osc(state: &str, msg: &str) -> String {
    format!("\x1b]7501;state={state}:app=rexsim:msg={}\x1b\\", b64(msg.as_bytes()))
}

// ---------- terminal ----------

#[derive(Clone, Copy, PartialEq, Debug, Default)]
struct WinSize {
    cols: u16,
    rows: u16,
    /// Pane size in pixels, the space mouse mode 1016 reports in.
    px_w: u16,
    px_h: u16,
}

fn winsize() -> WinSize {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) };
    WinSize { cols: ws.ws_col, rows: ws.ws_row, px_w: ws.ws_xpixel, px_h: ws.ws_ypixel }
}

struct RawMode(libc::termios);

impl RawMode {
    fn enter() -> RawMode {
        let mut t: libc::termios = unsafe { std::mem::zeroed() };
        unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut t) };
        let saved = t;
        unsafe { libc::cfmakeraw(&mut t) };
        unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &t) };
        RawMode(saved)
    }
}

const ENTER: &str = "\x1b[?1049h\x1b[?25l\x1b[2J\x1b[?1002h\x1b[?1006h\x1b[?1016h";
const LEAVE: &str = "\x1b_Ga=d,d=A,q=2\x1b\\\x1b[?1016l\x1b[?1006l\x1b[?1002l\x1b[?25h\x1b[?1049l";

impl Drop for RawMode {
    fn drop(&mut self) {
        let mut out = std::io::stdout();
        let _ = out.write_all(LEAVE.as_bytes());
        let _ = out.flush();
        unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.0) };
    }
}

/// Cell size in pixels from the ioctl, or by asking the terminal (CSI 16 t) when the
/// ioctl leaves pixels at zero. Runs before the input thread owns stdin.
fn cell_px(ws: WinSize) -> (f64, f64) {
    if ws.px_w > 0 && ws.px_h > 0 && ws.cols > 0 && ws.rows > 0 {
        return (ws.px_w as f64 / ws.cols as f64, ws.px_h as f64 / ws.rows as f64);
    }
    let mut out = std::io::stdout();
    let _ = out.write_all(b"\x1b[16t");
    let _ = out.flush();
    let mut buf = Vec::new();
    let deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < deadline && !buf.ends_with(b"t") {
        let mut pfd = libc::pollfd { fd: libc::STDIN_FILENO, events: libc::POLLIN, revents: 0 };
        if unsafe { libc::poll(&mut pfd, 1, 50) } > 0 {
            let mut b = [0u8; 64];
            let n = unsafe { libc::read(libc::STDIN_FILENO, b.as_mut_ptr().cast(), b.len()) };
            if n > 0 {
                buf.extend_from_slice(&b[..n as usize]);
            }
        }
    }
    // ESC [ 6 ; h ; w t
    let s = String::from_utf8_lossy(&buf);
    let nums: Vec<f64> = s
        .trim_start_matches("\x1b[")
        .trim_end_matches('t')
        .split(';')
        .filter_map(|n| n.parse().ok())
        .collect();
    match nums.as_slice() {
        [6.0, h, w] if *h > 0.0 && *w > 0.0 => (*w, *h),
        _ => (8.0, 16.0),
    }
}

// ---------- layout ----------

/// Where the image sits: a cell rectangle, and the same rectangle in pane pixels.
#[derive(Clone, Copy, Debug, Default)]
struct Layout {
    col0: u16, // 0-based
    row0: u16,
    cols: u16,
    rows: u16,
    px: (f64, f64, f64, f64), // x, y, w, h in pane pixels
}

/// Rows under the phone: the hardware toolbar, plus the stats line with `--stats`.
static CHROME_ROWS: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(1);
fn chrome_rows() -> u16 {
    CHROME_ROWS.load(Ordering::Relaxed)
}

/// Fit a `fw`×`fh` frame into the pane above the toolbar, centred, aspect kept as
/// closely as whole cells allow.
fn layout(ws: WinSize, cell: (f64, f64), fw: u32, fh: u32) -> Layout {
    let (cw, ch) = cell;
    let avail_rows = ws.rows.saturating_sub(chrome_rows()).max(1);
    let (aw, ah) = (ws.cols as f64 * cw, avail_rows as f64 * ch);
    let s = (aw / fw as f64).min(ah / fh as f64);
    let cols = ((fw as f64 * s / cw).round() as u16).clamp(1, ws.cols.max(1));
    let rows = ((fh as f64 * s / ch).round() as u16).clamp(1, avail_rows);
    let col0 = (ws.cols - cols) / 2;
    let row0 = (avail_rows - rows) / 2;
    Layout {
        col0,
        row0,
        cols,
        rows,
        px: (col0 as f64 * cw, row0 as f64 * ch, cols as f64 * cw, rows as f64 * ch),
    }
}

// ---------- hardware buttons ----------

#[derive(Clone, Copy, PartialEq, Debug)]
enum Hw {
    Home,
    Apps,
    Lock,
    Siri,
    VolDown,
    VolUp,
}

const HW: [(Hw, &str); 6] = [
    (Hw::Home, "Home"),
    (Hw::Apps, "Apps"),
    (Hw::Lock, "Lock"),
    (Hw::Siri, "Siri"),
    (Hw::VolDown, "Vol −"),
    (Hw::VolUp, "Vol +"),
];

/// A drawn button: cells `col0..col1` of `row`, 0-based.
#[derive(Clone, Copy, Debug)]
struct HwRect {
    hw: Hw,
    row: u16,
    col0: u16,
    col1: u16,
}

/// The toolbar row sits under the phone, centred on it, kept inside the pane.
fn toolbar(ws: WinSize, l: &Layout) -> Vec<HwRect> {
    let row = ws.rows.saturating_sub(chrome_rows());
    let widths: Vec<u16> = HW.iter().map(|(_, s)| s.chars().count() as u16 + 2).collect();
    let total: u16 = widths.iter().sum::<u16>() + (HW.len() as u16 - 1);
    let centre = l.col0 + l.cols / 2;
    let mut col = centre.saturating_sub(total / 2).min(ws.cols.saturating_sub(total));
    let mut out = Vec::new();
    for ((hw, _), w) in HW.iter().zip(widths) {
        out.push(HwRect { hw: *hw, row, col0: col, col1: col + w });
        col += w + 1;
    }
    out
}

fn draw_toolbar(out: &mut impl Write, ws: WinSize, rects: &[HwRect], pressed: Option<Hw>) {
    let Some(first) = rects.first() else { return };
    let _ = write!(out, "\x1b[{};1H\x1b[2K", first.row + 1);
    for (r, (_, label)) in rects.iter().zip(HW.iter()) {
        if r.col1 > ws.cols {
            break;
        }
        let style = if pressed == Some(r.hw) { "\x1b[7m" } else { "\x1b[48;5;237m\x1b[38;5;252m" };
        let _ = write!(out, "\x1b[{};{}H{style} {label} \x1b[0m", r.row + 1, r.col0 + 1);
    }
}

/// A click is one press; holding longer than half a second is a long press of that
/// length, like a physical button. Apps is Home pressed twice (the app switcher).
fn press_hw(driver: &Driver, hw: Hw, held: Duration) {
    let hold = if held > Duration::from_millis(500) { held.as_secs_f32() } else { 0.0 };
    match hw {
        Hw::Home => driver.button(Button::Home, 1, hold),
        Hw::Apps => driver.button(Button::Home, 2, 0.0),
        Hw::Lock => driver.button(Button::Lock, 1, hold),
        Hw::Siri => driver.button(Button::Siri, 1, hold),
        Hw::VolDown => driver.button(Button::VolumeDown, 1, hold),
        Hw::VolUp => driver.button(Button::VolumeUp, 1, hold),
    }
    log(format!("button {hw:?} hold={hold:.2}"));
}

// ---------- sticking to the phone's width ----------

/// Keeps the rexsim pane as wide as the phone needs by moving the border between it and
/// the pane it was opened from.
struct Stick {
    session: String,
    me: String,
    anchor: String,
    /// rexsim is right of the anchor (split right) rather than left of it.
    after: bool,
    last: Option<Instant>,
    tries: u32,
}

/// Columns the phone needs at this pane height, plus a column of margin each side.
fn phone_cols(ws: WinSize, cell: (f64, f64), fw: u32, fh: u32) -> u16 {
    let rows = ws.rows.saturating_sub(chrome_rows()).max(1) as f64;
    ((rows * cell.1 * fw as f64 / fh as f64) / cell.0).ceil() as u16 + 2
}

fn block_cols(block: &str) -> Option<f64> {
    let out = std::process::Command::new("rex").args(["block", "call", "-b", block, "size"]).output().ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    let rest = &s[s.find("\"columns\":")? + 10..];
    rest.trim_start().split(|c: char| !c.is_ascii_digit()).next()?.parse().ok()
}

impl Stick {
    fn from_args(anchor: Option<String>, side: Option<String>) -> Option<Stick> {
        let after = match side.as_deref()? {
            "right" => true,
            "left" => false,
            _ => return None, // above/below: width is the window's, nothing to hold
        };
        Some(Stick {
            session: std::env::var("REX_SESSION").ok()?,
            me: std::env::var("REX_BLOCK").ok()?,
            anchor: anchor?,
            after,
            last: None,
            tries: 0,
        })
    }

    /// Ask Rex to move the border so this pane is `target` columns wide. Runs `rex` on a
    /// thread so the frame loop never waits on it. Gives up after a few tries per size,
    /// when the window is too narrow to fit the phone.
    fn hold(&mut self, cols: u16, target: u16) {
        if cols.abs_diff(target) <= 1 {
            self.tries = 0;
            return;
        }
        if self.tries >= 4 || self.last.is_some_and(|t| t.elapsed() < Duration::from_millis(400)) {
            return;
        }
        self.tries += 1;
        self.last = Some(Instant::now());
        let (session, me, anchor, after) = (self.session.clone(), self.me.clone(), self.anchor.clone(), self.after);
        std::thread::spawn(move || {
            let Some(other) = block_cols(&anchor) else { return };
            let total = other + cols as f64;
            let mine = (target as f64 / total).clamp(0.1, 0.9);
            let (before, after_id, ratio) = if after { (&anchor, &me, 1.0 - mine) } else { (&me, &anchor, mine) };
            let args = format!(
                r#"{{"direction":"horizontal","before_block_id":"{before}","after_block_id":"{after_id}","ratio":{ratio:.4}}}"#
            );
            let ok = std::process::Command::new("rex")
                .args(["api", "call", "-s", &session, "session.resize_pane", &args])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok_and(|s| s.success());
            log(format!("stick: {cols} -> {target} cols, ratio {ratio:.3}, ok={ok}"));
        });
    }
}

// ---------- shared-memory frames ----------

fn b64(input: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::new();
    for c in input.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            if i <= c.len() {
                s.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                s.push('=');
            }
        }
    }
    s
}

struct ShmWriter {
    pid: u32,
    n: u64,
    /// Recent segment names. The terminal unlinks each one it reads; one still present
    /// several frames later was never read, so we unlink it and count a leak.
    recent: VecDeque<CString>,
    leaks: u64,
}

impl ShmWriter {
    /// Copy `frame` (BGRA) into a new segment as RGBA and return its name.
    fn write(&mut self, frame: &Frame) -> Option<CString> {
        self.n += 1;
        let name = CString::new(format!("/sdp{}.{}", self.pid, self.n)).ok()?;
        let len = frame.data.len();
        unsafe {
            let fd = libc::shm_open(name.as_ptr(), libc::O_CREAT | libc::O_EXCL | libc::O_RDWR, 0o600);
            if fd < 0 {
                return None;
            }
            if libc::ftruncate(fd, len as libc::off_t) != 0 {
                libc::close(fd);
                libc::shm_unlink(name.as_ptr());
                return None;
            }
            let p = libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd, 0);
            libc::close(fd);
            if p == libc::MAP_FAILED {
                libc::shm_unlink(name.as_ptr());
                return None;
            }
            let dst = std::slice::from_raw_parts_mut(p.cast::<u8>(), len);
            for (d, s) in dst.chunks_exact_mut(4).zip(frame.data.chunks_exact(4)) {
                d[0] = s[2];
                d[1] = s[1];
                d[2] = s[0];
                d[3] = 255;
            }
            libc::munmap(p, len);
        }
        self.recent.push_back(name.clone());
        while self.recent.len() > 64 {
            let old = self.recent.pop_front().unwrap();
            if unsafe { libc::shm_unlink(old.as_ptr()) } == 0 {
                self.leaks += 1;
                log(format!("dropped unread segment {old:?}"));
            }
        }
        Some(name)
    }

    /// The terminal answered: it has read (and should have unlinked) every segment sent.
    /// One still present was not unlinked by it; remove it and count it.
    fn settle(&mut self) {
        for n in self.recent.drain(..) {
            if unsafe { libc::shm_unlink(n.as_ptr()) } == 0 {
                self.leaks += 1;
                log(format!("terminal left segment {n:?} linked"));
            }
        }
    }

    fn cleanup(&mut self) {
        for n in self.recent.drain(..) {
            unsafe { libc::shm_unlink(n.as_ptr()) };
        }
    }
}

// ---------- input ----------

struct Shared {
    latest: Mutex<Option<Frame>>,
    wake: Condvar,
    quit: AtomicBool,
    state: Mutex<String>,
    layout: Mutex<Layout>,
    /// Set when the terminal answers a frame (OK or error): the next frame may go.
    acked: Mutex<bool>,
    ack_wake: Condvar,
    last_err: Mutex<String>,
    /// Set by the driver thread when its state changes; the main loop reports it.
    status_dirty: AtomicBool,
    cell: Mutex<(f64, f64)>,
    buttons: Mutex<Vec<HwRect>>,
    pressed: Mutex<Option<Hw>>,
    toolbar_dirty: AtomicBool,
}

fn norm(l: &Layout, x: f64, y: f64) -> (f64, f64) {
    let (x0, y0, w, h) = l.px;
    ((x - x0) / w.max(1.0), (y - y0) / h.max(1.0))
}

fn inside(p: (f64, f64)) -> bool {
    (0.0..=1.0).contains(&p.0) && (0.0..=1.0).contains(&p.1)
}

/// Reads stdin, parses SGR mouse reports and keys, and drives the simulator.
fn input_loop(driver: Arc<Driver>, sh: Arc<Shared>) {
    let mut buf: Vec<u8> = Vec::new();
    let mut down = false;
    let mut last_wheel: Option<Instant> = None;
    let mut wheel_at = (0.5, 0.5);
    let mut wheel_dy = 0.0;
    let mods = Modifiers::default();
    let mut btn_down: Option<(Hw, Instant)> = None;
    while !sh.quit.load(Ordering::Relaxed) {
        let mut pfd = libc::pollfd { fd: libc::STDIN_FILENO, events: libc::POLLIN, revents: 0 };
        if unsafe { libc::poll(&mut pfd, 1, 30) } > 0 {
            let mut b = [0u8; 4096];
            let n = unsafe { libc::read(libc::STDIN_FILENO, b.as_mut_ptr().cast(), b.len()) };
            if n <= 0 {
                break;
            }
            buf.extend_from_slice(&b[..n as usize]);
        }
        // A wheel burst ends after a short quiet spell.
        if let Some(t) = last_wheel {
            if t.elapsed() > Duration::from_millis(120) {
                driver.scroll(ScrollPhase::Ended, wheel_at.0, wheel_at.1, 0.0, wheel_dy);
                last_wheel = None;
            }
        }
        let mut i = 0;
        while i < buf.len() {
            let rest = &buf[i..];
            if rest.starts_with(b"\x1b_G") {
                // A graphics reply: ESC _ G i=1[,p=1] ; OK|error ESC \\
                let Some(end) = rest.windows(2).position(|w| w == b"\x1b\\") else { break };
                let body = String::from_utf8_lossy(&rest[3..end]).to_string();
                i += end + 2;
                log(format!("reply {body}"));
                if body.starts_with("i=1") || body.starts_with("i=2") {
                    let msg = body.split_once(';').map(|(_, m)| m).unwrap_or("");
                    if msg != "OK" {
                        *sh.last_err.lock().unwrap() = msg.to_string();
                    }
                    *sh.acked.lock().unwrap() = true;
                    sh.ack_wake.notify_all();
                }
                continue;
            }
            if rest.starts_with(b"\x1b[<") {
                // ESC [ < b ; x ; y (M|m)
                let Some(end) = rest.iter().position(|&c| c == b'M' || c == b'm') else { break };
                let body = String::from_utf8_lossy(&rest[3..end]).to_string();
                let press = rest[end] == b'M';
                i += end + 1;
                let f: Vec<f64> = body.split(';').filter_map(|s| s.parse().ok()).collect();
                let [b, x, y] = f[..] else { continue };
                let b = b as u32;
                let l = *sh.layout.lock().unwrap();
                let p = norm(&l, x - 1.0, y - 1.0);
                log(format!("mouse b={b} press={press} px=({x},{y}) norm=({:.3},{:.3})", p.0, p.1));
                if b == 0 && !down {
                    let (cw, ch) = *sh.cell.lock().unwrap();
                    let (col, row) = (((x - 1.0) / cw) as u16, ((y - 1.0) / ch) as u16);
                    let hit = sh.buttons.lock().unwrap().iter().find(|r| r.row == row && (r.col0..r.col1).contains(&col)).map(|r| r.hw);
                    if press {
                        if let Some(hw) = hit {
                            btn_down = Some((hw, Instant::now()));
                            *sh.pressed.lock().unwrap() = Some(hw);
                            sh.toolbar_dirty.store(true, Ordering::Relaxed);
                            sh.wake.notify_one();
                            continue;
                        }
                    } else if let Some((hw, t)) = btn_down.take() {
                        // Released anywhere: still a press of the button it started on.
                        press_hw(&driver, hw, t.elapsed());
                        *sh.pressed.lock().unwrap() = None;
                        sh.toolbar_dirty.store(true, Ordering::Relaxed);
                        sh.wake.notify_one();
                        continue;
                    }
                }
                if btn_down.is_some() {
                    continue; // a drag that started on a button isn't a touch
                }
                match b {
                    64 | 65 => {
                        if !inside(p) {
                            continue;
                        }
                        let dy = if b == 64 { 0.04 } else { -0.04 };
                        if last_wheel.is_none() {
                            wheel_at = p;
                            driver.scroll(ScrollPhase::Began, p.0, p.1, 0.0, 0.0);
                        }
                        wheel_dy = dy;
                        driver.scroll(ScrollPhase::Changed, wheel_at.0, wheel_at.1, 0.0, dy);
                        last_wheel = Some(Instant::now());
                    }
                    0 if press && !down && inside(p) => {
                        down = true;
                        driver.pointer(Phase::Down, p.0, p.1, mods);
                    }
                    0 if !press && down => {
                        down = false;
                        driver.pointer(Phase::Up, p.0, p.1, mods);
                    }
                    32 if down => driver.pointer(Phase::Move, p.0, p.1, mods),
                    _ => {}
                }
                continue;
            }
            if rest.starts_with(b"\x1b[") && rest.len() >= 3 {
                let key = match rest[2] {
                    b'A' => Some("ArrowUp"),
                    b'B' => Some("ArrowDown"),
                    b'C' => Some("ArrowRight"),
                    b'D' => Some("ArrowLeft"),
                    _ => None,
                };
                if let Some(k) = key {
                    driver.key(k);
                    i += 3;
                    continue;
                }
                // Some other CSI: skip to its final byte.
                let end = rest[2..].iter().position(|&c| (0x40..=0x7e).contains(&c));
                match end {
                    Some(e) => i += 2 + e + 1,
                    None => break,
                }
                continue;
            }
            match rest[0] {
                0x03 => {
                    sh.quit.store(true, Ordering::Relaxed);
                    sh.wake.notify_all();
                    return;
                }
                0x08 => driver.button(Button::Home, 1, 0.0),
                b'\r' | b'\n' => {
                    driver.key("Enter");
                }
                0x7f => {
                    driver.key("Backspace");
                }
                b'\t' => {
                    driver.key("Tab");
                }
                0x1b if rest.len() == 1 => {
                    driver.key("Escape");
                }
                0x1b => {} // lone ESC prefix of something unparsed
                c if c >= 0x20 => {
                    // A run of printable UTF-8.
                    let end = rest.iter().position(|&c| c < 0x20 || c == 0x7f).unwrap_or(rest.len());
                    let s = String::from_utf8_lossy(&rest[..end]).to_string();
                    driver.text(&s);
                    i += end;
                    continue;
                }
                _ => {}
            }
            i += 1;
        }
        buf.drain(..i.min(buf.len()));
    }
}

// ---------- main ----------

const USAGE: &str = "\
rexsim: a live iOS Simulator inside a Rex pane

Usage: rexsim [--udid <udid>] [--fps N] [--split right|left|above|below] [--here]
              [--no-stick] [--log <file>]

  --udid <udid>   Which simulator (default: the first booted one).
  --fps N         Frame rate cap (default 60).
  --split DIR     Where the new pane opens beside the current one (default right).
  --here          Draw in this terminal instead of opening a new pane.
  --no-stick      Let the pane be any width (by default it keeps to the phone's width).
  --stats         Show a line of frame rates under the buttons.
  --log <file>    Write a frame-by-frame timing log (also $REXSIM_LOG).

Inside the pane: click, drag and scroll to touch; type to send keys. The buttons
under the phone are Home, Apps (the app switcher), Lock, Siri and volume; hold
one for a long press. Ctrl-H presses Home; Ctrl-C quits.
";

/// From a Rex pane, open a split beside it running `rexsim --here` with the same options.
/// Returns false when this isn't a Rex terminal, so the caller draws here instead.
fn open_rex_pane(split: &str, passthrough: &[String]) -> bool {
    let Ok(block) = std::env::var("REX_BLOCK") else { return false };
    if std::env::var("TERM_PROGRAM").as_deref() != Ok("rex") {
        return false;
    }
    let exe = std::env::current_exe().unwrap_or_else(|_| "rexsim".into());
    let status = std::process::Command::new("rex")
        .args(["run", "-b", &block, "--split", split, "--ratio", "40", "--shell=none", "--"])
        .arg(&exe)
        .arg("--here")
        .args(passthrough)
        .stdout(std::process::Stdio::null())
        .status();
    match status {
        Ok(s) if s.success() => {
            println!("rexsim: simulator pane opened ({split})");
            true
        }
        Ok(s) => {
            eprintln!("rexsim: `rex run` failed ({s}); drawing here instead");
            false
        }
        Err(e) => {
            eprintln!("rexsim: couldn't run `rex` ({e}); drawing here instead");
            false
        }
    }
}

fn main() {
    let (mut udid, mut fps, mut here, mut split) = (None, 60u32, false, "right".to_string());
    let mut passthrough = Vec::new();
    let (mut anchor, mut side, mut no_stick) = (None::<String>, None::<String>, false);
    let mut stats = false;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--udid" => {
                udid = args.next();
                passthrough.extend(["--udid".to_string(), udid.clone().unwrap_or_default()]);
            }
            "--fps" => {
                fps = args.next().and_then(|s| s.parse().ok()).unwrap_or(60);
                passthrough.extend(["--fps".to_string(), fps.to_string()]);
            }
            "--split" => split = args.next().unwrap_or(split),
            "--log" => {
                let p = args.next().unwrap_or_default();
                // Absolute, so the new pane's working directory doesn't matter.
                let p = std::path::absolute(&p).map(|a| a.display().to_string()).unwrap_or(p);
                passthrough.extend(["--log".to_string(), p.clone()]);
                let _ = LOG_PATH.set(Some(p));
            }
            "--here" => here = true,
            "--stats" => {
                stats = true;
                passthrough.push("--stats".to_string());
            }
            "--no-stick" => {
                no_stick = true;
                passthrough.push("--no-stick".to_string());
            }
            // Set by the launching rexsim: the pane it split from, and which side we're on.
            "--anchor" => anchor = args.next(),
            "--side" => side = args.next(),
            "-h" | "--help" => {
                print!("{USAGE}");
                return;
            }
            other => {
                eprintln!("rexsim: unknown argument {other:?}\n\n{USAGE}");
                std::process::exit(2);
            }
        }
    }
    if stats {
        CHROME_ROWS.store(2, Ordering::Relaxed);
    }
    if LOG_PATH.get().is_none() {
        if let Ok(p) = std::env::var("REXSIM_LOG") {
            let p = std::path::absolute(&p).map(|a| a.display().to_string()).unwrap_or(p);
            passthrough.extend(["--log".to_string(), p.clone()]);
            let _ = LOG_PATH.set(Some(p));
        }
    }
    if !here {
        if let Ok(b) = std::env::var("REX_BLOCK") {
            passthrough.extend(["--anchor".to_string(), b, "--side".to_string(), split.clone()]);
        }
    }
    if !here && open_rex_pane(&split, &passthrough) {
        return;
    }

    let sh = Arc::new(Shared {
        latest: Mutex::new(None),
        wake: Condvar::new(),
        quit: AtomicBool::new(false),
        state: Mutex::new("starting".into()),
        layout: Mutex::new(Layout::default()),
        acked: Mutex::new(true),
        ack_wake: Condvar::new(),
        last_err: Mutex::new(String::new()),
        status_dirty: AtomicBool::new(true),
        cell: Mutex::new((8.0, 16.0)),
        buttons: Mutex::new(Vec::new()),
        pressed: Mutex::new(None),
        toolbar_dirty: AtomicBool::new(false),
    });
    T0.get_or_init(Instant::now);
    LOG.get_or_init(|| {
        LOG_PATH.get().cloned().flatten().and_then(|p| std::fs::File::create(p).ok()).map(Mutex::new)
    });

    let cb = sh.clone();
    let opts = Options {
        device: udid.map_or(DeviceSelector::FirstBootedOrIphone, DeviceSelector::Udid),
        fps,
        ..Options::default()
    };
    let driver = Driver::start(opts, move |ev| match ev {
        Event::Frame(f) => {
            *cb.latest.lock().unwrap() = Some(f);
            cb.wake.notify_one();
        }
        Event::State(s) => {
            *cb.state.lock().unwrap() = match s {
                State::Starting => "starting",
                State::Live => "live",
                State::InputDisplaced => "input taken elsewhere",
                State::Restarting => "restarting",
                State::Stopped => "stopped",
            }
            .into();
            log(format!("driver state {s:?}"));
            cb.status_dirty.store(true, Ordering::Relaxed);
            cb.wake.notify_one();
        }
        Event::Note(n) => log(format!("driver note {n}")),
    })
    .unwrap_or_else(|e| {
        print!("{}", status_osc("error", &format!("simdriver: {e}")));
        eprintln!("Driver::start: {e} (is a simulator booted?)");
        std::process::exit(1);
    });
    let driver = Arc::new(driver);

    let raw = RawMode::enter();
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(ENTER.as_bytes());
    let _ = out.flush();

    let mut ws = winsize();
    let mut cell = cell_px(ws);

    let input = {
        let (d, s) = (driver.clone(), sh.clone());
        std::thread::spawn(move || input_loop(d, s))
    };

    let mut stick = if no_stick { None } else { Stick::from_args(anchor, side) };
    let mut shm = ShmWriter { pid: std::process::id(), n: 0, recent: VecDeque::new(), leaks: 0 };
    let mut last: Option<Frame> = None;
    let mut frame_dims = (0u32, 0u32);
    let mut dirty = true;
    let mut sent = 0u64;
    let mut sent_window = (Instant::now(), 0u64, 0.0f64);
    let mut next_status = Instant::now();

    while !sh.quit.load(Ordering::Relaxed) {
        let fresh = {
            let mut g = sh.latest.lock().unwrap();
            if g.is_none() {
                g = sh.wake.wait_timeout(g, Duration::from_millis(250)).unwrap().0;
            }
            g.take()
        };
        if sh.quit.load(Ordering::Relaxed) {
            break;
        }

        let now_ws = winsize();
        if now_ws != ws {
            ws = now_ws;
            if ws.px_w > 0 && ws.cols > 0 {
                cell = (ws.px_w as f64 / ws.cols as f64, ws.px_h as f64 / ws.rows as f64);
            }
            dirty = true;
        }
        let mut send_now = dirty;
        if let Some(f) = fresh {
            last = Some(f);
            send_now = true;
        }
        if send_now && last.is_some() {
            // One frame in flight: wait for the terminal's reply to the previous one.
            // Frames that arrive meanwhile replace `latest`, so the newest is what goes next.
            // Segments are only unlinked after a real reply: unlinking on a timeout would
            // pull a frame out from under a terminal that is merely slow.
            let t = Instant::now();
            let mut a = sh.acked.lock().unwrap();
            let deadline = t + Duration::from_secs(2);
            while !*a && Instant::now() < deadline && !sh.quit.load(Ordering::Relaxed) {
                a = sh.ack_wake.wait_timeout(a, Duration::from_millis(50)).unwrap().0;
            }
            let acked = *a;
            *a = false;
            drop(a);
            let waited = t.elapsed().as_secs_f64() * 1000.0;
            if acked {
                shm.settle();
            } else {
                log(format!("ack timeout after {waited:.0} ms, {} unread", shm.recent.len()));
            }
            if let Some(newer) = sh.latest.lock().unwrap().take() {
                last = Some(newer);
            }
            if waited > 20.0 {
                log(format!("waited {waited:.1} ms for ack"));
            }
        }
        if let Some(f) = last.as_ref().filter(|_| send_now) {
            if (f.width, f.height) != frame_dims {
                frame_dims = (f.width, f.height);
                dirty = true;
            }
            if dirty {
                *sh.layout.lock().unwrap() = layout(ws, cell, f.width, f.height);
                log(format!(
                    "layout ws={ws:?} cell={cell:?} frame={}x{} data={} (expect {}) {:?}",
                    f.width,
                    f.height,
                    f.data.len(),
                    f.width as usize * f.height as usize * 4,
                    *sh.layout.lock().unwrap()
                ));
                let _ = out.write_all(b"\x1b[2J\x1b_Ga=d,d=A,q=2\x1b\\");
                dirty = false;
                *sh.cell.lock().unwrap() = cell;
                *sh.buttons.lock().unwrap() = toolbar(ws, &sh.layout.lock().unwrap());
                sh.toolbar_dirty.store(true, Ordering::Relaxed);
                next_status = Instant::now(); // the clear wiped the status line too
                if let Some(st) = stick.as_mut() {
                    st.hold(ws.cols, phone_cols(ws, cell, f.width, f.height));
                }
            }
            let l = *sh.layout.lock().unwrap();
            if let Some(name) = shm.write(f) {
                // Double-buffered: place the new frame under a second id (one z above the
                // old), then delete the old image in the same write, so the terminal never
                // renders a gap between the two.
                let (new_id, old_id) = if sent % 2 == 0 { (1, 2) } else { (2, 1) };
                let _ = write!(
                    out,
                    "\x1b[?2026h\x1b[{};{}H\x1b_Ga=T,t=s,f=32,s={},v={},i={},p=1,z={},c={},r={},C=1,q=0;{}\x1b\\\x1b_Ga=d,d=I,i={},q=2\x1b\\\x1b[?2026l",
                    l.row0 + 1,
                    l.col0 + 1,
                    f.width,
                    f.height,
                    new_id,
                    sent % 1_000_000,
                    l.cols,
                    l.rows,
                    b64(name.as_bytes()),
                    old_id,
                );
                sent += 1;
                log(format!("sent #{sent} id={new_id} seq={}", f.seq));
            }
        }
        if sh.toolbar_dirty.swap(false, Ordering::Relaxed) {
            let pressed = *sh.pressed.lock().unwrap();
            draw_toolbar(&mut out, ws, &sh.buttons.lock().unwrap(), pressed);
        }
        if sh.status_dirty.swap(false, Ordering::Relaxed) {
            let st = sh.state.lock().unwrap().clone();
            let (state, msg) = match st.as_str() {
                "live" => ("idle", "Simulator live".to_string()),
                "input taken elsewhere" => ("working", "Agent is driving the simulator".to_string()),
                "restarting" => ("working", "Simulator restarting".to_string()),
                "stopped" => ("error", "Simulator stopped".to_string()),
                _ => ("working", "Connecting to simulator".to_string()),
            };
            let _ = out.write_all(status_osc(state, &msg).as_bytes());
        }
        if Instant::now() >= next_status {
            let (t0, n0, _) = sent_window;
            let rate = (sent - n0) as f64 / t0.elapsed().as_secs_f64().max(0.001);
            sent_window = (Instant::now(), sent, rate);
            next_status = Instant::now() + Duration::from_secs(1);
            let st = driver.stats();
            let line = format!(
                " rexsim · {} · {} · sim {:.0} fps · drawn {:.0} fps · {}×{} · leaks {}{} · ^C quit ^H home",
                &driver.udid()[..8.min(driver.udid().len())],
                sh.state.lock().unwrap(),
                st.fps,
                rate,
                frame_dims.0,
                frame_dims.1,
                shm.leaks,
                { let e = sh.last_err.lock().unwrap(); if e.is_empty() { String::new() } else { format!(" · err {e}") } },
            );
            if stats {
                let line: String = line.chars().take(ws.cols as usize).collect();
                let _ = write!(out, "\x1b[{};1H\x1b[2K\x1b[2m{}\x1b[0m", ws.rows, line);
            } else if rate > 0.0 || st.fps > 0.0 {
                log(format!("stats{}", line.trim_start_matches(" rexsim")));
            }
        }
        let _ = out.flush();
    }

    sh.quit.store(true, Ordering::Relaxed);
    let _ = input.join();
    shm.cleanup();
    let _ = out.write_all(b"\x1b]7501;state=clear\x1b\\");
    let _ = out.flush();
    drop(out);
    drop(raw);
    if let Ok(d) = Arc::try_unwrap(driver) {
        d.stop();
    }
}
