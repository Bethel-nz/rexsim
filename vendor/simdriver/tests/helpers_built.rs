#[test]
fn both_helpers_exist_and_print_usage() {
    for name in ["sdframe", "sdhid"] {
        let bin = simdriver::baked_helper_dir().join(name);
        assert!(bin.exists(), "{bin:?} missing");
        let out = std::process::Command::new(&bin).output().unwrap();
        assert_eq!(
            out.status.code(),
            Some(2),
            "{name} with no args must exit 2 (usage)"
        );
    }
}
