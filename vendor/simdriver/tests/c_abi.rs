//! Task 10: the committed header is what cbindgen generates, and a C program compiled
//! against the cdylib can call the ABI. No simulator needed — only local error paths.
use std::path::{Path, PathBuf};
use std::process::Command;

fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn workspace_target_debug() -> PathBuf {
    // crates/simdriver -> workspace root -> target/debug. Honour CARGO_TARGET_DIR.
    if let Ok(t) = std::env::var("CARGO_TARGET_DIR") {
        return PathBuf::from(t).join("debug");
    }
    crate_dir()
        .ancestors()
        .nth(2)
        .unwrap()
        .join("target")
        .join("debug")
}

fn generate_to(path: &Path) {
    let config = cbindgen::Config::from_file(crate_dir().join("cbindgen.toml"))
        .expect("read crates/simdriver/cbindgen.toml");
    cbindgen::Builder::new()
        .with_config(config)
        .with_crate(crate_dir())
        .generate()
        .expect("cbindgen generate (see cbindgen.toml)")
        .write_to_file(path);
}

#[test]
fn header_is_current() {
    let tmp = std::env::temp_dir().join(format!("simdriver-header-{}.h", std::process::id()));
    generate_to(&tmp);
    let committed_path = crate_dir().join("include/simdriver.h");
    let committed = std::fs::read_to_string(&committed_path).unwrap_or_else(|e| {
        panic!(
            "cannot read {}: {e}; regenerate with `cargo run -p simdriver --example gen_header`",
            committed_path.display()
        )
    });
    let fresh = std::fs::read_to_string(&tmp).unwrap();
    let _ = std::fs::remove_file(&tmp);
    assert_eq!(
        committed, fresh,
        "include/simdriver.h is stale; regenerate with `cargo run -p simdriver --example gen_header`"
    );
}

#[test]
fn c_smoke() {
    let debug = workspace_target_debug();
    let dylib = debug.join("libsimdriver.dylib");
    // Always build: cargo test does not necessarily build the cdylib, and an existing one
    // may be stale (testing it would miss ffi.rs changes). A no-op when it is current; the
    // outer cargo has released the target-dir lock by the time tests run.
    let st = Command::new(env!("CARGO"))
        .args(["build", "-p", "simdriver", "--lib"])
        .status()
        .expect("run cargo build for the cdylib");
    assert!(st.success(), "cargo build -p simdriver --lib failed");
    assert!(dylib.is_file(), "{} missing after build", dylib.display());

    let out = debug.join(format!("c_smoke-{}", std::process::id()));
    let cc = Command::new("clang")
        .args([
            crate_dir()
                .join("tests/c/smoke.c")
                .as_os_str()
                .to_string_lossy()
                .as_ref(),
            "-I",
            crate_dir().join("include").to_str().unwrap(),
            "-L",
            debug.to_str().unwrap(),
            "-lsimdriver",
            "-o",
            out.to_str().unwrap(),
        ])
        .status()
        .expect("run clang (Xcode CLT must be installed)");
    assert!(cc.success(), "clang could not compile tests/c/smoke.c");

    let run = Command::new(&out)
        .env("DYLD_LIBRARY_PATH", &debug)
        .output()
        .expect("run the smoke binary");
    let stdout = String::from_utf8_lossy(&run.stdout);
    let stderr = String::from_utf8_lossy(&run.stderr);
    let _ = std::fs::remove_file(&out);
    assert!(
        run.status.success(),
        "smoke.c failed\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(stdout.contains("smoke ok"), "unexpected output:\n{stdout}");
}
