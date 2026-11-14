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
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

// ---------- log ----------

static T0: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
/// When to grab the home screen as the pane's wallpaper: at start, and just after Home.
static WALL_DUE: Mutex<Option<Instant>> = Mutex::new(None);
/// Bumped a moment after Home, so rexsim's own wallpaper is redrawn.
static WALL_GEN: AtomicU32 = AtomicU32::new(0);
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
        unpublish_bar();
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

/// Rows under the phone: the stats line with `--stats`, else none.
static CHROME_ROWS: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(0);
/// Rows above the phone: the glass button bar (two rows) and a row of air.
const TOP_ROWS: u16 = 5;
fn chrome_rows() -> u16 {
    CHROME_ROWS.load(Ordering::Relaxed)
}

/// Fit a `fw`×`fh` frame into the pane above the toolbar, centred, aspect kept as
/// closely as whole cells allow.
fn layout(ws: WinSize, cell: (f64, f64), fw: u32, fh: u32) -> Layout {
    let (cw, ch) = cell;
    // The button bar and a gap on top, two rows of air (room for the frame) at the bottom.
    let avail_rows = ws.rows.saturating_sub(TOP_ROWS + chrome_rows() + 2).max(1);
    let (aw, ah) = (ws.cols as f64 * cw, avail_rows as f64 * ch);
    let s = (aw / fw as f64).min(ah / fh as f64);
    let cols = ((fw as f64 * s / cw).round() as u16).clamp(1, ws.cols.max(1));
    let rows = ((fh as f64 * s / ch).round() as u16).clamp(1, avail_rows);
    let col0 = (ws.cols - cols) / 2;
    let row0 = TOP_ROWS + (avail_rows - rows) / 2;
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
    (Hw::VolDown, "Vol−"),
    (Hw::VolUp, "Vol+"),
];

/// A button's cells: columns `col0..col1` of rows `row..=row1`, 0-based.
#[derive(Clone, Copy, Debug)]
struct HwRect {
    hw: Hw,
    row: u16,
    row1: u16,
    col0: u16,
    col1: u16,
}

/// The glass bar spans the phone's width in the top two rows. Button slots match the
/// drawing in bar.swift: equal slots inside the capsule's rounded ends.
fn toolbar(_ws: WinSize, l: &Layout, cell: (f64, f64)) -> Vec<HwRect> {
    let (cw, ch) = cell;
    let w = l.cols as f64 * cw;
    let inset = 6.0;
    let bar_h = 2.0 * ch - inset * 2.0;
    let slot = (w - inset * 2.0 - bar_h * 0.4) / HW.len() as f64;
    let x0 = inset + bar_h * 0.2;
    HW.iter()
        .enumerate()
        .map(|(i, (hw, _))| HwRect {
            hw: *hw,
            row: 0,
            row1: 1,
            col0: l.col0 + ((x0 + slot * i as f64) / cw) as u16,
            col1: l.col0 + ((x0 + slot * (i + 1) as f64) / cw).ceil() as u16,
        })
        .collect()
}

/// Where the compiled bar renderer lives; built from bar.swift on first use.
fn bar_tool() -> Option<std::path::PathBuf> {
    let dir = std::path::PathBuf::from(std::env::var("HOME").ok()?).join("Library/Caches/rexsim");
    let bin = dir.join("bar-v1");
    if bin.exists() {
        return Some(bin);
    }
    std::fs::create_dir_all(&dir).ok()?;
    let src = dir.join("bar.swift");
    std::fs::write(&src, include_str!("bar.swift")).ok()?;
    let ok = std::process::Command::new("swiftc")
        .args(["-O", src.to_str()?, "-o", bin.to_str()?])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    ok.then_some(bin)
}

/// The scene helper (src/chrome.swift), compiled once like the bar.
fn chrome_tool() -> Option<std::path::PathBuf> {
    let dir = std::path::PathBuf::from(std::env::var("HOME").ok()?).join("Library/Application Support/rexsim/bin");
    let bin = dir.join("chrome-v1");
    if bin.exists() {
        return Some(bin);
    }
    std::fs::create_dir_all(&dir).ok()?;
    let src = dir.join("chrome.swift");
    std::fs::write(&src, include_str!("chrome.swift")).ok()?;
    let ok = std::process::Command::new("swiftc")
        .args(["-O", src.to_str()?, "-o", bin.to_str()?])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    ok.then_some(bin)
}

fn side_button(name: &str) -> Option<Button> {
    Some(match name {
        "action" => Button::Action,
        "volup" => Button::VolumeUp,
        "voldown" => Button::VolumeDown,
        "lock" => Button::Lock,
        _ => return None,
    })
}

/// A side button on the drawn frame, in pane pixels.
#[derive(Clone, Debug)]
struct Side {
    name: String,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

/// The wallpaper and frame PNGs for a pane of `pw`×`ph` pixels with the screen at
/// `screen`, plus the frame's side buttons.
fn scene(pw: u32, ph: u32, screen: (f64, f64, f64, f64)) -> Option<(Vec<u8>, Vec<u8>, Vec<Side>)> {
    let tool = chrome_tool()?;
    let dir = tool.parent()?.to_path_buf();
    let (bg, fr) = (dir.join("bg.png"), dir.join("frame.png"));
    let out = std::process::Command::new(&tool)
        .args([
            pw.to_string(),
            ph.to_string(),
            format!("{:.1}", screen.0),
            format!("{:.1}", screen.1),
            format!("{:.1}", screen.2),
            format!("{:.1}", screen.3),
            bg.to_str()?.into(),
            fr.to_str()?.into(),
        ])
        .output()
        .ok()?;
    let sides = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            let n = |i: usize| f.get(i)?.parse::<f64>().ok();
            (f.first() == Some(&"btn")).then(|| Some(Side { name: f.get(1)?.to_string(), x: n(2)?, y: n(3)?, w: n(4)?, h: n(5)? }))?
        })
        .collect();
    Some((std::fs::read(bg).ok()?, std::fs::read(fr).ok()?, sides))
}

/// The bar as PNG bytes for this size and pressed button, cached on disk.
fn bar_png(w: u32, h: u32, pressed: Option<usize>) -> Option<Vec<u8>> {
    let tool = bar_tool()?;
    let p = pressed.map_or(-1, |i| i as i64);
    let out = tool.with_file_name(format!("bar-{w}x{h}-{p}.png"));
    if !out.exists() {
        std::process::Command::new(&tool)
            .args([w.to_string(), h.to_string(), p.to_string(), "2".into(), out.to_str()?.into()])
            .status()
            .ok()?;
    }
    std::fs::read(out).ok()
}

/// Sends a PNG in 4 KB base64 chunks (the graphics protocol's limit per escape).
fn send_png(out: &mut impl Write, id: u32, row: u16, col: u16, cols: u16, rows: u16, z: i32, png: &[u8]) {
    let data = b64(png);
    let chunks: Vec<&str> = data.as_bytes().chunks(4096).map(|c| std::str::from_utf8(c).unwrap()).collect();
    let _ = write!(out, "\x1b[{};{}H", row + 1, col + 1);
    for (i, c) in chunks.iter().enumerate() {
        let more = if i + 1 < chunks.len() { 1 } else { 0 };
        if i == 0 {
            let _ = write!(out, "\x1b_Ga=T,f=100,i={id},p=1,z={z},c={cols},r={rows},C=1,q=2,m={more};{c}\x1b\\");
        } else {
            let _ = write!(out, "\x1b_Gm={more};{c}\x1b\\");
        }
    }
}

/// With Hydra in Rex, the buttons are native Liquid Glass drawn over the pane, so rexsim
/// only says where the bar goes (a file Hydra reads) and takes presses from a FIFO.
fn native_bar() -> bool {
    // REXSIM_BUILTIN=1 draws rexsim's own frame and wallpaper even with Hydra installed.
    if std::env::var_os("REXSIM_BUILTIN").is_some() {
        return false;
    }
    std::env::var("HOME").is_ok_and(|h| std::path::Path::new(&format!("{h}/.hydra/Hydra.dylib")).exists())
}

fn panes_dir() -> Option<std::path::PathBuf> {
    let d = std::path::PathBuf::from(std::env::var("HOME").ok()?).join("Library/Application Support/rexsim/panes");
    std::fs::create_dir_all(&d).ok()?;
    Some(d)
}

/// Tells Hydra which pane this is and where the bar sits in it, in cells.
fn publish_bar(l: &Layout, cell: (f64, f64)) {
    let (Some(dir), Ok(block), Ok(session)) = (panes_dir(), std::env::var("REX_BLOCK"), std::env::var("REX_SESSION")) else { return };
    let pid = std::process::id();
    let json = format!(
        r#"{{"pid":{pid},"block":"{block}","session":"{session}","fifo":"{}","col0":{},"row0":0,"cols":{},"rows":{},"cw":{},"ch":{},"px":[{},{},{},{}],"grid":[{},{}]}}"#,
        dir.join(format!("{pid}.fifo")).display(),
        l.col0,
        l.cols,
        l.row0,
        cell.0,
        cell.1,
        l.px.0,
        l.px.1,
        l.px.2,
        l.px.3,
        { let w = winsize(); w.cols as f64 * cell.0 },
        { let w = winsize(); w.rows as f64 * cell.1 }
    );
    let _ = std::fs::write(dir.join(format!("{pid}.json")), json);
}

/// Reads "home", "apps", "lock", "siri", "voldown" or "volup", each optionally with a
/// hold time in seconds, one per line, from Hydra's buttons.
fn listen_bar(driver: Arc<Driver>) {
    let Some(dir) = panes_dir() else { return };
    let path = dir.join(format!("{}.fifo", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let Ok(c) = CString::new(path.to_string_lossy().as_bytes()) else { return };
    if unsafe { libc::mkfifo(c.as_ptr(), 0o600) } != 0 {
        return;
    }
    std::thread::spawn(move || {
        use std::io::BufRead;
        loop {
            let Ok(f) = std::fs::File::open(&path) else { return };
            for line in std::io::BufReader::new(f).lines().map_while(Result::ok) {
                let mut it = line.split_whitespace();
                let head = it.next();
                if head == Some("down") || head == Some("up") {
                    // Live press: "down volup" when the mouse goes down, "up volup" when it lifts.
                    let b = match it.next() {
                        Some("volup") => Some(Button::VolumeUp),
                        Some("voldown") => Some(Button::VolumeDown),
                        Some("lock") => Some(Button::Lock),
                        Some("action") => Some(Button::Action),
                        Some("home") => Some(Button::Home),
                        Some("siri") => Some(Button::Siri),
                        _ => None,
                    };
                    if let Some(b) = b {
                        driver.button_state(b, head == Some("down"));
                    }
                    continue;
                }
                if head == Some("touch") {
                    // "touch down|move|up x y" straight from Rex, 0..1: no trip through the terminal.
                    let ph = it.next();
                    let v: Vec<f64> = it.filter_map(|s| s.parse().ok()).collect();
                    if let [x, y] = v[..] {
                        let phase = match ph {
                            Some("down") => Phase::Down,
                            Some("up") => Phase::Up,
                            _ => Phase::Move,
                        };
                        driver.pointer(phase, x, y, Modifiers::default());
                    }
                    continue;
                }
                if head == Some("scroll") {
                    // "scroll began|changed|ended|momentum x y dx dy" from the trackpad, all 0..1.
                    let ph = match it.next() {
                        Some("began") => ScrollPhase::Began,
                        Some("ended") => ScrollPhase::Ended,
                        Some("momentum") => ScrollPhase::Momentum,
                        _ => ScrollPhase::Changed,
                    };
                    let v: Vec<f64> = it.filter_map(|s| s.parse().ok()).collect();
                    if let [x, y, dx, dy] = v[..] {
                        driver.scroll(ph, x, y, dx, dy);
                    }
                    continue;
                }
                let hw = match head {
                    Some("home") => Hw::Home,
                    Some("apps") => Hw::Apps,
                    Some("lock") => Hw::Lock,
                    Some("siri") => Hw::Siri,
                    Some("voldown") => Hw::VolDown,
                    Some("volup") => Hw::VolUp,
                    Some("action") => {
                        driver.button(Button::Action, 1, it.next().and_then(|s| s.parse::<f32>().ok()).unwrap_or(0.0));
                        continue;
                    }
                    _ => continue,
                };
                let held = it.next().and_then(|s| s.parse::<f32>().ok()).unwrap_or(0.0);
                press_hw(&driver, hw, Duration::from_secs_f32(held));
            }
        }
    });
}

/// A 120-pixel-wide copy of the screen for Hydra's glow behind the pane:
/// "w h\n" then RGBA. Written to a temp file and renamed, so Hydra never reads half.
fn write_glow(f: &Frame) {
    let Some(dir) = panes_dir() else { return };
    let (w, h) = (f.width as usize, f.height as usize);
    let gw = 120usize;
    let gh = (gw * h / w.max(1)).max(1);
    let mut out = format!("{gw} {gh}\n").into_bytes();
    for y in 0..gh {
        for x in 0..gw {
            // Average the whole block, so the picture is smooth rather than speckled.
            let (x0, x1, y0, y1) = (x * w / gw, ((x + 1) * w / gw).max(x * w / gw + 1), y * h / gh, ((y + 1) * h / gh).max(y * h / gh + 1));
            let (mut r, mut g, mut b, mut n) = (0u32, 0u32, 0u32, 0u32);
            for yy in (y0..y1).step_by(2) {
                for xx in (x0..x1).step_by(2) {
                    let i = (yy * w + xx) * 4;
                    b += f.data[i] as u32;
                    g += f.data[i + 1] as u32;
                    r += f.data[i + 2] as u32;
                    n += 1;
                }
            }
            let n = n.max(1);
            out.extend_from_slice(&[(r / n) as u8, (g / n) as u8, (b / n) as u8, 255]);
        }
    }
    let pid = std::process::id();
    let tmp = dir.join(format!("{pid}.glow.tmp"));
    if std::fs::write(&tmp, out).is_ok() {
        let _ = std::fs::rename(tmp, dir.join(format!("{pid}.glow")));
    }
}

fn unpublish_bar() {
    if let Some(dir) = panes_dir() {
        let pid = std::process::id();
        let _ = std::fs::remove_file(dir.join(format!("{pid}.json")));
        let _ = std::fs::remove_file(dir.join(format!("{pid}.fifo")));
        let _ = std::fs::remove_file(dir.join(format!("{pid}.glow")));
    }
}

fn draw_toolbar(out: &mut impl Write, l: &Layout, cell: (f64, f64), pressed: Option<Hw>) {
    if native_bar() {
        publish_bar(l, cell);
        return;
    }
    let w = (l.cols as f64 * cell.0).round() as u32;
    let h = (2.0 * cell.1).round() as u32;
    let idx = pressed.and_then(|p| HW.iter().position(|(hw, _)| *hw == p));
    if let Some(png) = bar_png(w, h, idx) {
        send_png(out, 10, 0, l.col0, l.cols, 2, 5, &png);
    }
}

/// A click is one press; holding longer than half a second is a long press of that
/// length, like a physical button. Apps is Home pressed twice (the app switcher).
fn press_hw(driver: &Driver, hw: Hw, held: Duration) {
    let hold = if held > Duration::from_millis(500) { held.as_secs_f32() } else { 0.0 };
    match hw {
        Hw::Home => {
            driver.button(Button::Home, 1, hold);
            *WALL_DUE.lock().unwrap() = Some(Instant::now() + Duration::from_millis(1200));
            std::thread::spawn(|| {
                std::thread::sleep(Duration::from_millis(1500));
                WALL_GEN.fetch_add(1, Ordering::Relaxed);
            });
        }
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
    let rows = ws.rows.saturating_sub(TOP_ROWS + chrome_rows() + 2).max(1) as f64;
    // The phone plus a little margin each side.
    ((rows * cell.1 * fw as f64 / fh as f64) / cell.0).ceil() as u16 + 6
}

/// The other pane in this pane's window, for when the pane rexsim was opened beside has
/// since been replaced.
fn neighbour(session: &str, me: &str) -> Option<String> {
    let out = std::process::Command::new("rex").args(["block", "ls", "-s", session, "--json"]).output().ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    let field = |part: &str, key: &str| -> Option<String> {
        let i = part.find(&format!("\"{key}\": \""))? + key.len() + 5;
        Some(part[i..].split('"').next()?.to_string())
    };
    let blocks: Vec<(String, String)> = s
        .split("{")
        .filter_map(|p| Some((field(p, "block_id")?, field(p, "window_id")?)))
        .collect();
    let win = blocks.iter().find(|(b, _)| b == me)?.1.clone();
    blocks.into_iter().find(|(b, w)| *w == win && b != me).map(|(b, _)| b)
}

fn block_cols(session: &str, block: &str) -> Option<f64> {
    let out = std::process::Command::new("rex").args(["block", "call", "-s", session, "-b", block, "size"]).output().ok()?;
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
            let (anchor, other) = match block_cols(&session, &anchor) {
                Some(c) => (anchor, c),
                None => {
                    let Some(n) = neighbour(&session, &me) else { return };
                    let Some(c) = block_cols(&session, &n) else { return };
                    (n, c)
                }
            };
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

/// Turns a BGRA frame into RGBA at no more than the size it's shown at, on every core:
/// the simulator's frames are about 3x the pane's pixels, and sending them full size
/// is most of the per-frame cost. Box-averages `k`x`k` blocks, so text stays crisp.
fn prepare(f: &Frame, shown_w: f64) -> (Vec<u8>, usize, usize) {
    let (w, h) = (f.width as usize, f.height as usize);
    let k = ((w as f64 / shown_w.max(1.0)).floor() as usize).clamp(1, 4);
    let (w2, h2) = (w / k, h / k);
    let mut out = vec![0u8; w2 * h2 * 4];
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).min(8);
    let band = h2.div_ceil(threads);
    let src = &f.data;
    std::thread::scope(|sc| {
        for (b, chunk) in out.chunks_mut(band * w2 * 4).enumerate() {
            sc.spawn(move || {
                let y0 = b * band;
                for (dy, row) in chunk.chunks_exact_mut(w2 * 4).enumerate() {
                    let y = (y0 + dy) * k;
                    for x in 0..w2 {
                        let (mut r, mut g, mut bl) = (0u32, 0u32, 0u32);
                        for yy in y..y + k {
                            let base = (yy * w + x * k) * 4;
                            for xx in 0..k {
                                let i = base + xx * 4;
                                bl += src[i] as u32;
                                g += src[i + 1] as u32;
                                r += src[i + 2] as u32;
                            }
                        }
                        let n = (k * k) as u32;
                        let o = x * 4;
                        row[o] = (r / n) as u8;
                        row[o + 1] = (g / n) as u8;
                        row[o + 2] = (bl / n) as u8;
                        row[o + 3] = 255;
                    }
                }
            });
        }
    });
    round_corners(&mut out, w2, h2);
    (out, w2, h2)
}

/// The phone's screen, blown up, blurred and dimmed to fill the pane around it, like the
/// ambient glow behind a video. `pw`x`ph` is the pane's shape; the result is small and the
/// terminal stretches it.
fn backdrop(f: &Frame, pw: f64, ph: f64) -> (Vec<u8>, usize, usize) {
    let bw = 72usize;
    let bh = ((bw as f64 * ph / pw.max(1.0)).round() as usize).clamp(8, 400);
    let (w, h) = (f.width as f64, f.height as f64);
    // Aspect-fill the frame into the backdrop.
    let s = (bw as f64 / w).max(bh as f64 / h);
    let (ox, oy) = ((w * s - bw as f64) / 2.0, (h * s - bh as f64) / 2.0);
    let mut img = vec![0f32; bw * bh * 3];
    for y in 0..bh {
        for x in 0..bw {
            let sx = (((x as f64 + ox) / s) as usize).min(f.width as usize - 1);
            let sy = (((y as f64 + oy) / s) as usize).min(f.height as usize - 1);
            let i = (sy * f.width as usize + sx) * 4;
            let o = (y * bw + x) * 3;
            img[o] = f.data[i + 2] as f32;
            img[o + 1] = f.data[i + 1] as f32;
            img[o + 2] = f.data[i] as f32;
        }
    }
    // Three box blurs make a soft, Gaussian-like glow.
    let rad = 5i64;
    for _ in 0..3 {
        for pass in 0..2 {
            let src = img.clone();
            for y in 0..bh as i64 {
                for x in 0..bw as i64 {
                    let mut acc = [0f32; 3];
                    let mut n = 0f32;
                    for d in -rad..=rad {
                        let (sx, sy) = if pass == 0 { (x + d, y) } else { (x, y + d) };
                        if sx < 0 || sy < 0 || sx >= bw as i64 || sy >= bh as i64 {
                            continue;
                        }
                        let i = (sy as usize * bw + sx as usize) * 3;
                        acc[0] += src[i];
                        acc[1] += src[i + 1];
                        acc[2] += src[i + 2];
                        n += 1.0;
                    }
                    let o = (y as usize * bw + x as usize) * 3;
                    img[o] = acc[0] / n;
                    img[o + 1] = acc[1] / n;
                    img[o + 2] = acc[2] / n;
                }
            }
        }
    }
    // Dim it so the phone stays the brightest thing in the pane.
    let mut out = vec![0u8; bw * bh * 4];
    for (o, c) in out.chunks_exact_mut(4).zip(img.chunks_exact(3)) {
        o[0] = (c[0] * 0.55) as u8;
        o[1] = (c[1] * 0.55) as u8;
        o[2] = (c[2] * 0.55) as u8;
        o[3] = 235;
    }
    (out, bw, bh)
}

/// Cuts the frame's corners to the phone's own curve (about 13.7% of its width on
/// current iPhones), with a one-pixel soft edge so it doesn't look jagged.
/// The iPhone 17 Pro screen's corner, measured from the Apple Frames picture the pane's
/// device frame uses: for each row from the top of a 1206-pixel-wide screen, how many
/// pixels are cut off at the side. Using the same curve keeps the screen flush with the frame.
const CORNER: [u16; 219] = [211,190,178,170,163,156,150,145,141,137,133,129,126,123,121,118,115,112,110,108,106,103,101,99,97,95,93,91,90,88,86,85,83,81,80,79,77,76,74,73,72,70,69,68,67,65,64,63,62,61,60,59,58,57,56,55,54,53,52,51,50,49,48,47,47,45,45,44,43,42,41,41,40,39,38,38,37,36,36,35,34,34,33,32,32,31,30,30,29,29,28,27,27,27,26,26,25,24,24,23,23,22,22,21,21,21,20,20,19,19,18,18,17,17,17,16,16,16,15,15,15,14,14,14,13,13,13,12,12,12,11,11,11,11,10,10,10,10,9,9,9,9,8,8,8,8,7,7,7,7,6,6,6,6,6,6,6,5,5,5,5,5,5,5,4,4,4,4,4,4,4,3,3,3,3,3,3,3,3,3,2,2,2,2,2,2,2,2,2,2,2,2,2,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,0];

fn round_corners(px: &mut [u8], w: usize, h: usize) {
    let k = w as f64 / 1206.0;
    // With Hydra the frame sits on top with its opening ~9 (of 1206) pixels inside the
    // screen, so trim along a curve that far outside it: hidden under the bezel.
    let o = if native_bar() { 9.0 } else { 0.0 };
    let rows = ((CORNER.len() as f64 - o).max(1.0) * k).ceil() as usize;
    if rows * 2 > h {
        return;
    }
    for y in 0..rows {
        // The cut at this row, between table rows, in this image's pixels.
        let t = y as f64 / k + o;
        let i = (t as usize).min(CORNER.len() - 1);
        let j = (i + 1).min(CORNER.len() - 1);
        let f = t - i as f64;
        let cut = ((CORNER[i] as f64 * (1.0 - f) + CORNER[j] as f64 * f) - o).max(0.0) * k;
        let full = cut.floor() as usize;
        for x in 0..=full.min(w / 2) {
            let a = if x < full { 0.0 } else { 1.0 - (cut - full as f64) };
            let alpha = (a * 255.0).round() as u8;
            for (cx, cy) in [(x, y), (w - 1 - x, y), (x, h - 1 - y), (w - 1 - x, h - 1 - y)] {
                let p = &mut px[(cy * w + cx) * 4 + 3];
                *p = (*p).min(alpha);
            }
        }
    }
}

impl ShmWriter {
    /// Put ready RGBA pixels into a new segment and return its name.
    fn write_rgba(&mut self, rgba: &[u8]) -> Option<CString> {
        self.n += 1;
        let name = CString::new(format!("/sdp{}.{}", self.pid, self.n)).ok()?;
        let len = rgba.len();
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
            std::ptr::copy_nonoverlapping(rgba.as_ptr(), p.cast::<u8>(), len);
            libc::munmap(p, len);
        }
        self.recent.push_back(name.clone());
        while self.recent.len() > 64 {
            let old = self.recent.pop_front().unwrap();
            if unsafe { libc::shm_unlink(old.as_ptr()) } == 0 {
                self.leaks += 1;
            }
        }
        Some(name)
    }

    /// Copy `frame` (BGRA) into a new segment as RGBA and return its name.
    #[allow(dead_code)]
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
            round_corners(dst, frame.width as usize, frame.height as usize);
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
    /// The drawn frame's side buttons, for clicks (pane pixels).
    sides: Mutex<Vec<Side>>,
    /// A finished scene waiting to be sent: (key, wallpaper, frame).
    scene_ready: Mutex<Option<(String, Vec<u8>, Vec<u8>)>>,
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
    let mut side_down: Option<Button> = None;
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
                // A click on the drawn frame's side buttons presses that button, live.
                if b == 0 && !down {
                    let (px, py) = (x - 1.0, y - 1.0);
                    if press {
                        let hit = sh.sides.lock().unwrap().iter()
                            .find(|s| px >= s.x - 6.0 && px <= s.x + s.w + 6.0 && py >= s.y && py <= s.y + s.h)
                            .map(|s| s.name.clone());
                        if let Some(name) = hit.and_then(|n| side_button(&n)) {
                            side_down = Some(name);
                            driver.button_state(name, true);
                            continue;
                        }
                    } else if let Some(name) = side_down.take() {
                        driver.button_state(name, false);
                        continue;
                    }
                }
                if b == 0 && !down {
                    let (cw, ch) = *sh.cell.lock().unwrap();
                    let (col, row) = (((x - 1.0) / cw) as u16, ((y - 1.0) / ch) as u16);
                    let hit = sh.buttons.lock().unwrap().iter().find(|r| (r.row..=r.row1).contains(&row) && (r.col0..r.col1).contains(&col)).map(|r| r.hw);
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
                    if let Some(dir) = panes_dir() {
                        let _ = std::fs::remove_file(dir.join("../restore"));
                    }
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
        CHROME_ROWS.store(1, Ordering::Relaxed);
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
        sides: Mutex::new(Vec::new()),
        scene_ready: Mutex::new(None),
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
    if native_bar() {
        listen_bar(driver.clone());
    }

    // Remember this pane so Rex can bring it back after a relaunch. Ctrl-C (a real quit)
    // removes it; Rex closing the pane under us leaves it.
    if let (Some(dir), Ok(session)) = (panes_dir(), std::env::var("REX_SESSION")) {
        let a = anchor.as_deref().unwrap_or("-");
        let _ = std::fs::write(dir.join("../restore"), format!("{session}\n{a}\n{}\n", side.as_deref().unwrap_or("right")));
    }
    let mut stick = if no_stick { None } else { Stick::from_args(anchor, side) };
    let mut shm = ShmWriter { pid: std::process::id(), n: 0, recent: VecDeque::new(), leaks: 0 };
    let mut last: Option<Frame> = None;
    let mut frame_dims = (0u32, 0u32);
    let mut dirty = true;
    let mut sent = 0u64;
    let mut sent_window = (Instant::now(), 0u64, 0.0f64);
    let mut next_status = Instant::now();
    let mut wall_done = false;
    let mut scene_key = String::new();
    let scene_busy = Arc::new(AtomicBool::new(false));
    let mut next_backdrop = Instant::now();

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
                *sh.buttons.lock().unwrap() = toolbar(ws, &sh.layout.lock().unwrap(), cell);
                next_backdrop = Instant::now();
                sh.toolbar_dirty.store(true, Ordering::Relaxed);
                next_status = Instant::now(); // the clear wiped the status line too
                if let Some(st) = stick.as_mut() {
                    st.hold(ws.cols, phone_cols(ws, cell, f.width, f.height));
                }
            }
            let l = *sh.layout.lock().unwrap();
            let (rgba, iw, ih) = prepare(f, l.px.2);
            if let Some(name) = shm.write_rgba(&rgba) {
                // Double-buffered: place the new frame under a second id (one z above the
                // old), then delete the old image in the same write, so the terminal never
                // renders a gap between the two.
                let (new_id, old_id) = if sent % 2 == 0 { (1, 2) } else { (2, 1) };
                let _ = write!(
                    out,
                    "\x1b[?2026h\x1b[{};{}H\x1b_Ga=T,t=s,f=32,s={},v={},i={},p=1,z={},c={},r={},C=1,q=0;{}\x1b\\\x1b_Ga=d,d=I,i={},q=2\x1b\\\x1b[?2026l",
                    l.row0 + 1,
                    l.col0 + 1,
                    iw,
                    ih,
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
            // Without a Rex mod drawing native chrome, rexsim draws its own: the phone's
            // wallpaper behind the pane and a real device frame over the screen. Rebuilt
            // in the background when the pane or phone moves, or after Home (new wallpaper).
            if !native_bar() {
                let (pw, ph) = ((ws.cols as f64 * cell.0) as u32, (ws.rows as f64 * cell.1) as u32);
                let wall_gen = WALL_GEN.load(Ordering::Relaxed);
                let key = format!("{pw}x{ph}:{:?}:{wall_gen}", l.px);
                if key != scene_key && !scene_busy.load(Ordering::Relaxed) {
                    scene_key = key.clone();
                    scene_busy.store(true, Ordering::Relaxed);
                    let (sh2, busy) = (sh.clone(), scene_busy.clone());
                    let px = l.px;
                    std::thread::spawn(move || {
                        if let Some((bg, fr, sides)) = scene(pw, ph, px) {
                            *sh2.sides.lock().unwrap() = sides;
                            *sh2.scene_ready.lock().unwrap() = Some((key, bg, fr));
                        }
                        busy.store(false, Ordering::Relaxed);
                    });
                }
                if let Some((k, bg, fr)) = sh.scene_ready.lock().unwrap().take() {
                    if k == scene_key {
                        let _ = write!(out, "\x1b[?2026h\x1b7");
                        send_png(&mut out, 3, 0, 0, ws.cols, ws.rows, -1, &bg);
                        send_png(&mut out, 4, 0, 0, ws.cols, ws.rows, 2_000_000_000, &fr);
                        let _ = write!(out, "\x1b8\x1b[?2026l");
                    }
                }
            }
            // The glow around the phone: a few times a second is plenty.
            let due = WALL_DUE.lock().unwrap().is_some_and(|t| Instant::now() >= t);
            if native_bar() && (due || !wall_done) {
                *WALL_DUE.lock().unwrap() = None;
                wall_done = true;
                let _ = next_backdrop;
                write_glow(f);
            } else if false && Instant::now() >= next_backdrop {
                next_backdrop = Instant::now() + Duration::from_millis(250);
                let (bg, bw, bh) = backdrop(f, ws.cols as f64 * cell.0, ws.rows as f64 * cell.1);
                if let Some(name) = shm.write_rgba(&bg) {
                    let _ = write!(
                        out,
                        "\x1b[?2026h\x1b7\x1b[1;1H\x1b_Ga=T,t=s,f=32,s={bw},v={bh},i=3,p=1,z=-1,c={},r={},C=1,q=2;{}\x1b\\\x1b8\x1b[?2026l",
                        ws.cols,
                        ws.rows,
                        b64(name.as_bytes()),
                    );
                }
            }
        }
        if sh.toolbar_dirty.swap(false, Ordering::Relaxed) {
            let pressed = *sh.pressed.lock().unwrap();
            let l = *sh.layout.lock().unwrap();
            draw_toolbar(&mut out, &l, cell, pressed);
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
