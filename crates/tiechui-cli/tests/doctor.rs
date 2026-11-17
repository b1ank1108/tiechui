#![allow(clippy::unwrap_used)]
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

use serde_json::json;
use sha2::{Digest, Sha256};

fn doctor(prefix: &Path, profile: &Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_chui"))
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
    let binary = debug.join("chui");
    fs::hard_link(env!("CARGO_BIN_EXE_chui"), &binary).unwrap();
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
            "schema_version": 1, "files": {"chui": entry, "tiechui": entry}
        }))
        .unwrap(),
    )
    .unwrap();
    for name in ["chui", "tiechui"] {
        let path = prefix.join("bin").join(name);
        fs::write(&path, b"binary").unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    fs::create_dir(&profile).unwrap();
    fs::set_permissions(&profile, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(doctor(&prefix, &profile).status.success());
    fs::write(prefix.join("bin/chui"), b"tampered").unwrap();
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
    assert_eq!(fs::read(prefix.join("bin/chui")).unwrap(), b"tampered");
}

#[test]
fn doctor_accepts_managed_worktree_user_files_and_symlinks_while_checking_worktrees_boundary() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().unwrap();
    let debug = root.path().join("target/debug");
    fs::create_dir_all(&debug).unwrap();
    let binary = debug.join("chui");
    fs::hard_link(env!("CARGO_BIN_EXE_chui"), &binary).unwrap();

    let shared_env = root.path().join("shared.env");
    fs::write(&shared_env, "APP_ENV=dev\n").unwrap();

    let profile = root.path().join("profile");
    let worktrees = profile.join("worktrees");
    let bucket = worktrees.join("demo-repo-1234abcd");
    let wt = bucket.join("20261116-ef012345");
    fs::create_dir_all(&wt).unwrap();
    for dir in [&profile, &worktrees, &bucket, &wt] {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).unwrap();
    }

    let registry = worktrees.join("registry.json");
    fs::write(
        &registry,
        serde_json::to_vec_pretty(&json!({
            "version": 1,
            "worktrees": [{
                "name": "20261116-ef012345",
                "path": wt.display().to_string(),
                "project_root": root.path().join("demo-repo").display().to_string(),
                "branch": "tiechui/20261116-ef012345",
                "base_branch": "main",
                "created_at": 1
            }]
        }))
        .unwrap(),
    )
    .unwrap();
    fs::set_permissions(&registry, fs::Permissions::from_mode(0o600)).unwrap();

    let node_bin = wt.join("node_modules/.bin");
    fs::create_dir_all(&node_bin).unwrap();
    fs::set_permissions(wt.join("node_modules"), fs::Permissions::from_mode(0o755)).unwrap();
    fs::set_permissions(&node_bin, fs::Permissions::from_mode(0o755)).unwrap();
    symlink(&shared_env, wt.join(".env")).unwrap();

    let healthy = Command::new(&binary)
        .env_clear()
        .current_dir(&debug)
        .args(["doctor", "--profile"])
        .arg(&profile)
        .output()
        .unwrap();
    let healthy_out = String::from_utf8(healthy.stdout).unwrap();
    assert_eq!(healthy.status.code(), Some(0), "{healthy_out}");
    assert!(healthy_out.contains("Doctor: OK"), "{healthy_out}");

    fs::set_permissions(&worktrees, fs::Permissions::from_mode(0o755)).unwrap();
    fs::set_permissions(&registry, fs::Permissions::from_mode(0o644)).unwrap();
    let broken = Command::new(&binary)
        .env_clear()
        .current_dir(&debug)
        .args(["doctor", "--profile"])
        .arg(&profile)
        .output()
        .unwrap();
    let broken_out = String::from_utf8(broken.stdout).unwrap();
    assert_eq!(broken.status.code(), Some(2), "{broken_out}");
    assert!(
        broken_out.contains("worktrees: expected permissions 0700"),
        "{broken_out}"
    );
    assert!(
        broken_out.contains("registry.json: expected permissions 0600"),
        "{broken_out}"
    );
    assert!(!broken_out.contains("node_modules"), "{broken_out}");
    assert!(!broken_out.contains(".env"), "{broken_out}");
}

#[test]
fn doctor_checks_scratch_root_and_entries_while_ignoring_interior_user_files() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().unwrap();
    let debug = root.path().join("target/debug");
    fs::create_dir_all(&debug).unwrap();
    let binary = debug.join("chui");
    fs::hard_link(env!("CARGO_BIN_EXE_chui"), &binary).unwrap();

    let external_file = root.path().join("external.txt");
    fs::write(&external_file, "hello\n").unwrap();

    let profile = root.path().join("profile");
    let scratch = profile.join("scratch");
    let entry = scratch.join("2026-11-17-测试脚本-0123abcd");
    let sub = entry.join("sub");
    fs::create_dir_all(&sub).unwrap();
    for dir in [&profile, &scratch, &entry] {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).unwrap();
    }
    fs::set_permissions(&sub, fs::Permissions::from_mode(0o755)).unwrap();
    symlink(&external_file, entry.join("linked.txt")).unwrap();

    let run_doctor = || {
        let out = Command::new(&binary)
            .env_clear()
            .current_dir(&debug)
            .args(["doctor", "--profile"])
            .arg(&profile)
            .output()
            .unwrap();
        (out.status.code(), String::from_utf8(out.stdout).unwrap())
    };

    let (code, ok_out) = run_doctor();
    assert_eq!(code, Some(0), "{ok_out}");
    assert!(ok_out.contains("Doctor: OK"), "{ok_out}");

    fs::set_permissions(&entry, fs::Permissions::from_mode(0o755)).unwrap();
    let (code, bad_entry_out) = run_doctor();
    assert_eq!(code, Some(2), "{bad_entry_out}");
    assert!(
        bad_entry_out.contains("2026-11-17-测试脚本-0123abcd: expected permissions 0700"),
        "{bad_entry_out}"
    );
    assert!(!bad_entry_out.contains("/sub:"), "{bad_entry_out}");
    assert!(!bad_entry_out.contains("linked.txt"), "{bad_entry_out}");

    fs::set_permissions(&entry, fs::Permissions::from_mode(0o700)).unwrap();
    fs::set_permissions(&scratch, fs::Permissions::from_mode(0o755)).unwrap();
    let (code, bad_root_out) = run_doctor();
    assert_eq!(code, Some(2), "{bad_root_out}");
    assert!(
        bad_root_out.contains("scratch: expected permissions 0700"),
        "{bad_root_out}"
    );
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
