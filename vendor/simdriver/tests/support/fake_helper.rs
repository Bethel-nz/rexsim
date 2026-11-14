//! A stand-in for `sdframe` / `sdhid` in unit tests (built as the `fake_helper` example).
//!
//! Modes:
//!   echo               copy stdin to stdout chunk by chunk (one write per read), exit 0 on EOF.
//!                      Used to feed scripted sdframe lines: whatever the test writes in one
//!                      `send_line` comes back out of the helper as one burst.
//!   hid <dir>          sdhid stand-in: waits for `<dir>/go` to exist, prints
//!                      {"t":"ready","w":393,"h":852}, then appends every stdin line to `<dir>/log`.
//!                      `{"t":"_invalidate"}` prints {"t":"displaced"}; while displaced, a gesture
//!                      start (touch/touch2 down, button, key, text) prints {"t":"reclaimed"}.
//!                      `{"t":"_exit","code":N}` prints "fake_helper: exiting N" on stderr and
//!                      exits with N. Exit 0 on stdin EOF.
//!   lines <code> <l>…  print each argument as a line, then exit with <code>.
//!   fail <code> <msg> [ms]  print <msg> on stderr, then exit with <code> (after <ms>).
//!   frames <name> <slots> <cap>
//!                      sdframe stand-in over a segment the test created and filled (seqs 2,
//!                      4, 6 in slots 0, 1, 2): prints the `shm` line, then announces those
//!                      three frames in turn every 16 ms. `{"t":"_exit","code":N}` on stdin
//!                      exits with N (stderr "fake_helper: exiting N"); stdin EOF exits 0.
//!   stubborn           ignore SIGINT and stdin EOF; sleep until killed.
//!   ax [unsupported|hang]  sdax stand-in: prints {"t":"ready"}, then answers each
//!                      {"t":"snapshot","id":N} with a fixed two-node tree under id N.
//!                      `unsupported`: print an error line and exit 4 at once; `hang`:
//!                      never answer a snapshot.
use std::io::{BufRead, Read, Write};
use std::path::PathBuf;
use std::time::Duration;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("echo") => echo(),
        Some("hid") => hid(PathBuf::from(&args[1])),
        Some("lines") => {
            let code: i32 = args[1].parse().unwrap();
            let mut out = std::io::stdout().lock();
            for l in &args[2..] {
                writeln!(out, "{l}").unwrap();
            }
            out.flush().unwrap();
            std::process::exit(code);
        }
        Some("fail") => {
            if let Some(ms) = args.get(3) {
                std::thread::sleep(Duration::from_millis(ms.parse().unwrap()));
            }
            eprintln!("{}", args[2]);
            std::process::exit(args[1].parse().unwrap());
        }
        Some("frames") => frames(&args[1], &args[2], &args[3]),
        Some("ax") => ax(args.get(1).map(String::as_str)),
        Some("stubborn") => {
            unsafe {
                libc::signal(libc::SIGINT, libc::SIG_IGN);
            }
            println!("{{\"t\":\"note\",\"detail\":\"stubborn\"}}");
            loop {
                std::thread::sleep(Duration::from_secs(60));
            }
        }
        other => {
            eprintln!("fake_helper: unknown mode {other:?}");
            std::process::exit(2);
        }
    }
}

fn echo() {
    let mut inp = std::io::stdin().lock();
    let mut out = std::io::stdout().lock();
    let mut buf = [0u8; 65536];
    loop {
        match inp.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => {
                if out.write_all(&buf[..n]).and_then(|_| out.flush()).is_err() {
                    return;
                }
            }
        }
    }
}

/// `{"t":"_exit","code":N}` -> N.
fn exit_code(line: &str) -> Option<i32> {
    line.strip_prefix(r#"{"t":"_exit","code":"#)?
        .trim_end_matches('}')
        .parse()
        .ok()
}

fn frames(name: &str, slots: &str, cap: &str) {
    std::thread::spawn(|| {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            if let Some(code) = exit_code(&line) {
                eprintln!("fake_helper: exiting {code}");
                std::process::exit(code);
            }
        }
        std::process::exit(0);
    });
    let mut out = std::io::stdout().lock();
    writeln!(
        out,
        r#"{{"t":"shm","name":"{name}","slots":{slots},"cap":{cap}}}"#
    )
    .unwrap();
    loop {
        for (slot, seq) in [(0, 2), (1, 4), (2, 6)] {
            if writeln!(out, r#"{{"t":"frame","slot":{slot},"seq":{seq}}}"#)
                .and_then(|_| out.flush())
                .is_err()
            {
                std::process::exit(0);
            }
            std::thread::sleep(Duration::from_millis(16));
        }
    }
}

fn hid(dir: PathBuf) {
    while !dir.join("go").exists() {
        std::thread::sleep(Duration::from_millis(5));
    }
    let mut out = std::io::stdout().lock();
    writeln!(out, r#"{{"t":"ready","w":393,"h":852}}"#).unwrap();
    out.flush().unwrap();
    let mut log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("log"))
        .unwrap();
    let mut displaced = false;
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { return };
        if line.contains(r#""t":"_invalidate""#) {
            displaced = true;
            writeln!(out, r#"{{"t":"displaced"}}"#).unwrap();
            out.flush().unwrap();
            continue;
        }
        if let Some(code) = exit_code(&line) {
            eprintln!("fake_helper: exiting {code}");
            std::process::exit(code);
        }
        let start = line.contains(r#""p":"down""#)
            || [r#""t":"button""#, r#""t":"key""#, r#""t":"text""#]
                .iter()
                .any(|t| line.contains(t));
        if displaced && start {
            displaced = false;
            writeln!(out, r#"{{"t":"reclaimed"}}"#).unwrap();
            out.flush().unwrap();
        }
        writeln!(log, "{line}").unwrap();
        log.flush().unwrap();
    }
}

fn ax(mode: Option<&str>) {
    let mut out = std::io::stdout().lock();
    if mode == Some("unsupported") {
        writeln!(out, r#"{{"t":"error","detail":"AXPTranslator lacks frontmostApplicationWithDisplayId:bridgeDelegateToken:"}}"#).unwrap();
        out.flush().unwrap();
        std::process::exit(4);
    }
    writeln!(out, r#"{{"t":"ready"}}"#).unwrap();
    out.flush().unwrap();
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { return };
        if mode == Some("hang") || !line.contains(r#""t":"snapshot""#) {
            continue;
        }
        let id = line
            .split(r#""id":"#)
            .nth(1)
            .and_then(|r| r.trim_end_matches('}').parse::<u64>().ok())
            .unwrap_or(0);
        writeln!(
            out,
            r#"{{"t":"ax","id":{id},"ms":1.0,"requests":4,"root":{{"role":"AXApplication","label":"Fake","enabled":true,"frame":[0,0,393,852],"children":[{{"role":"AXButton","label":"Continue","id":"go","enabled":true,"frame":[20,700,353,50]}}]}}}}"#
        )
        .unwrap();
        out.flush().unwrap();
    }
}
