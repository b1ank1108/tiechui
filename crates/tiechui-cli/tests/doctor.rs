#![allow(clippy::unwrap_used)]
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

use serde_json::json;
use sha2::{Digest, Sha256};

fn doctor(prefix: &Path, profile: &Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_tc"))
        .env_clear()
        .args(["doctor", "--prefix"])
        .arg(prefix)
        .arg("--profile")
        .arg(profile)
        .output()
        .unwrap()
}

#[test]
fn doctor_without_prefix_skips_uninstalled_binary_checks() {
    let root = tempfile::tempdir().unwrap();
    let debug = root.path().join("target/debug");
    fs::create_dir_all(&debug).unwrap();
    let binary = debug.join("tc");
    fs::hard_link(env!("CARGO_BIN_EXE_tc"), &binary).unwrap();
    let profile = root.path().join("profile");
    fs::create_dir(&profile).unwrap();
    fs::set_permissions(&profile, fs::Permissions::from_mode(0o700)).unwrap();

    for healthy in [true, false] {
        if !healthy {
            fs::write(profile.join("config.toml"), "schema_version = 999\n").unwrap();
            let providers = profile.join("providers.toml");
            fs::write(
                &providers,
                "[fixture]\nwire = 'anthropic-messages'\napi_key = 'SENTINEL_DO_NOT_PRINT'\n",
            )
            .unwrap();
            fs::set_permissions(providers, fs::Permissions::from_mode(0o644)).unwrap();
        }
        let result = Command::new(&binary)
            .env_clear()
            .current_dir(&debug)
            .args(["doctor", "--profile"])
            .arg(&profile)
            .output()
            .unwrap();
        assert_eq!(result.status.code(), Some(if healthy { 0 } else { 2 }));
        let output = String::from_utf8(result.stdout).unwrap();
        assert_eq!(
            output
                .lines()
                .filter(|line| *line
                    == "INFO not installed by `cargo xtask install`; binary checks skipped")
                .count(),
            1,
            "{output}"
        );
        assert!(!output.contains("ownership.json"), "{output}");
        if healthy {
            assert!(output.contains("Doctor: OK"), "{output}");
            assert!(!output.contains("FAIL"), "{output}");
        } else {
            assert!(output.contains("invalid config schema"), "{output}");
            assert!(
                output.contains("providers.toml: expected permissions 0600"),
                "{output}"
            );
            assert!(!output.contains("SENTINEL"), "{output}");
        }
    }
}

#[test]
fn doctor_with_explicit_prefix_reports_missing_ownership_record() {
    let root = tempfile::tempdir().unwrap();
    let prefix = root.path().join("prefix");
    let profile = root.path().join("profile");
    fs::create_dir(&profile).unwrap();
    fs::set_permissions(&profile, fs::Permissions::from_mode(0o700)).unwrap();

    for existing_prefix in [false, true] {
        if existing_prefix {
            fs::create_dir_all(prefix.join("bin")).unwrap();
            fs::create_dir(prefix.join(".tiechui-install")).unwrap();
        }
        let result = doctor(&prefix, &profile);
        assert_eq!(result.status.code(), Some(2));
        let output = String::from_utf8(result.stdout).unwrap();
        assert!(
            output.contains(&format!(
                "FAIL {}: missing ownership record for prefix {}",
                prefix.join(".tiechui-install/ownership.json").display(),
                prefix.display()
            )),
            "{output}"
        );
        assert!(!output.contains("os error"), "{output}");
        assert!(!output.contains("binary checks skipped"), "{output}");
    }
}

#[test]
fn doctor_reports_broken_install_profile_and_schemas_without_changes() {
    let root = tempfile::tempdir().unwrap();
    let prefix = root.path().join("prefix");
    let profile = root.path().join("profile");
    fs::create_dir_all(prefix.join("bin")).unwrap();
    fs::create_dir(prefix.join(".tiechui-install")).unwrap();
    let hash = format!("{:x}", Sha256::digest(b"binary"));
    let entry = json!({"sha256": hash, "mode": 493});
    fs::write(
        prefix.join(".tiechui-install/ownership.json"),
        serde_json::to_vec(&json!({
            "schema_version": 1, "files": {"tc": entry, "tiechui": entry}
        }))
        .unwrap(),
    )
    .unwrap();
    for name in ["tc", "tiechui"] {
        let path = prefix.join("bin").join(name);
        fs::write(&path, b"binary").unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    fs::create_dir(&profile).unwrap();
    fs::set_permissions(&profile, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(doctor(&prefix, &profile).status.success());
    fs::write(prefix.join("bin/tc"), b"tampered").unwrap();
    fs::remove_file(prefix.join("bin/tiechui")).unwrap();
    fs::write(prefix.join(".tiechui-install/journal.json"), b"{}").unwrap();
    fs::set_permissions(&profile, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(profile.join("config.toml"), "schema_version = 999\n").unwrap();
    fs::write(profile.join("providers.toml"), "[fixture]\nmodels = []\n").unwrap();
    fs::write(profile.join("secrets.env"), "SENTINEL_DO_NOT_PRINT").unwrap();
    fs::set_permissions(
        profile.join("secrets.env"),
        fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    fs::create_dir(profile.join("mcp-oauth")).unwrap();
    fs::write(
        profile.join("mcp-oauth/fixture.json"),
        "SENTINEL_DO_NOT_PRINT",
    )
    .unwrap();
    fs::set_permissions(
        profile.join("mcp-oauth/fixture.json"),
        fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    fs::create_dir(profile.join("sessions")).unwrap();
    fs::write(
        profile.join("sessions/bad.jsonl"),
        r#"{"format_version":999,"id":"test","cwd":"/test","created_at":0}"#,
    )
    .unwrap();
    fs::write(profile.join("trust.json"), r#"{"schema_version":999}"#).unwrap();
    fs::create_dir(profile.join("plugin-grants")).unwrap();
    fs::write(
        profile.join("plugin-grants/bad.json"),
        r#"{"schema_version":999}"#,
    )
    .unwrap();
    let before: Vec<_> = walk(&profile);
    let result = doctor(&prefix, &profile);
    assert!(!result.status.success());
    let output = String::from_utf8(result.stdout).unwrap();
    for expected in [
        "checksum mismatch",
        "tiechui",
        "interrupted install",
        "0700",
        "0600",
        "config schema",
        "providers schema",
        "format_version",
        "trust.json",
        "plugin grant schema",
        "mcp-oauth",
    ] {
        assert!(output.contains(expected), "missing {expected}: {output}");
    }
    assert!(!output.contains("SENTINEL"));
    assert_eq!(before, walk(&profile));
    assert_eq!(fs::read(prefix.join("bin/tc")).unwrap(), b"tampered");
}

fn walk(path: &Path) -> Vec<(std::path::PathBuf, Vec<u8>, u32)> {
    let mut result = vec![];
    for entry in fs::read_dir(path).unwrap() {
        let path = entry.unwrap().path();
        let meta = fs::metadata(&path).unwrap();
        if meta.is_dir() {
            result.extend(walk(&path));
        } else {
            result.push((
                path.clone(),
                fs::read(path).unwrap(),
                meta.permissions().mode(),
            ));
        }
    }
    result.sort();
    result
}
