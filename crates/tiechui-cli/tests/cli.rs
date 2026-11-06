use std::process::Command;

const BINARIES: [(&str, &str); 2] = [
    ("chui", env!("CARGO_BIN_EXE_chui")),
    ("tiechui", env!("CARGO_BIN_EXE_tiechui")),
];

#[test]
fn help_uses_the_invoked_name() {
    for (name, binary) in BINARIES {
        let output = Command::new(binary).arg("--help").output().unwrap();
        assert!(output.status.success(), "{name}: {output:?}");
        assert!(output.stderr.is_empty());
        let help = String::from_utf8(output.stdout).unwrap();
        assert!(
            help.contains(&format!("Usage: {name} [OPTIONS] [COMMAND]")),
            "{name}: {help}"
        );
        assert!(help.contains("--help"));
        assert!(help.contains("--version"));
        assert!(help.contains("--no-alt-screen"));
    }
}

#[test]
fn version_uses_the_invoked_name() {
    for (name, binary) in BINARIES {
        let output = Command::new(binary).arg("--version").output().unwrap();
        assert!(output.status.success(), "{name}: {output:?}");
        assert!(output.stderr.is_empty());
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            format!("{name} {}\n", env!("CARGO_PKG_VERSION"))
        );
    }
}
