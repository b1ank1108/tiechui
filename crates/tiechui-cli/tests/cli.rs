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

#[cfg(all(windows, target_env = "msvc"))]
#[test]
fn windows_msvc_binaries_reserve_eight_mib_for_the_main_stack() {
    use std::io::{Read, Seek, SeekFrom};

    for (name, binary) in BINARIES {
        let mut file = std::fs::File::open(binary).unwrap();
        file.seek(SeekFrom::Start(0x3c)).unwrap();
        let mut offset = [0; 4];
        file.read_exact(&mut offset).unwrap();
        let pe = u64::from(u32::from_le_bytes(offset));
        file.seek(SeekFrom::Start(pe)).unwrap();
        let mut signature = [0; 4];
        file.read_exact(&mut signature).unwrap();
        assert_eq!(&signature, b"PE\0\0", "{name}");
        file.seek(SeekFrom::Start(pe + 24)).unwrap();
        let mut header = [0; 88];
        file.read_exact(&mut header).unwrap();
        let reserve = match u16::from_le_bytes(header[..2].try_into().unwrap()) {
            0x10b => u64::from(u32::from_le_bytes(header[72..76].try_into().unwrap())),
            0x20b => u64::from_le_bytes(header[72..80].try_into().unwrap()),
            magic => panic!("{name}: unexpected PE optional header {magic:#x}"),
        };
        assert_eq!(reserve, 8 * 1024 * 1024, "{name}");
    }
}
