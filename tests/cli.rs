use std::process::Command;

#[test]
fn version_flags_print_the_crate_version() {
    for flag in ["--version", "-V"] {
        let out = Command::new(env!("CARGO_BIN_EXE_gitst"))
            .arg(flag)
            .output()
            .unwrap();
        assert!(out.status.success(), "{flag}");
        assert_eq!(
            String::from_utf8(out.stdout).unwrap(),
            format!("gitst {}\n", env!("CARGO_PKG_VERSION")),
            "{flag}"
        );
    }
}
