//! Writes include/simdriver.h from the crate's FFI surface (cbindgen.toml is the config).
//! `tests/c_abi.rs::header_is_current` fails if the committed header drifts from this.
fn main() {
    let crate_dir = env!("CARGO_MANIFEST_DIR");
    let config = cbindgen::Config::from_file(format!("{crate_dir}/cbindgen.toml"))
        .expect("read cbindgen.toml");
    cbindgen::Builder::new()
        .with_config(config)
        .with_crate(crate_dir)
        .generate()
        .expect("cbindgen generate: nothing marked for export?")
        .write_to_file(format!("{crate_dir}/include/simdriver.h"));
}
