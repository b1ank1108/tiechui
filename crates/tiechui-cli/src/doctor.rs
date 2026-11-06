use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};

use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

pub fn run(args: &clap::ArgMatches) -> Result<()> {
    let profile = match args.get_one::<String>("profile") {
        Some(path) => PathBuf::from(path),
        None => tiechui_config::ConfigPaths::discover(&std::env::current_dir()?)?
            .user
            .parent()
            .ok_or("profile has no parent")?
            .to_owned(),
    };
    let explicit_prefix = args.get_one::<String>("prefix").is_some();
    let prefix = match args.get_one::<String>("prefix") {
        Some(path) => PathBuf::from(path),
        None => std::env::current_exe()?
            .parent()
            .and_then(Path::parent)
            .ok_or("executable has no prefix")?
            .to_owned(),
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let cancel = CancellationToken::new();
    let (result, binary_checks_skipped) = runtime.block_on(async {
        let worker_cancel = cancel.clone();
        let mut worker = tokio::task::spawn_blocking(move || {
            inspect(&prefix, explicit_prefix, &profile, &worker_cancel)
        });
        tokio::select! {
            result = &mut worker => result?,
            _ = tokio::signal::ctrl_c() => {
                cancel.cancel();
                worker.await?
            }
        }
    })?;
    let mut output = std::io::stdout().lock();
    if binary_checks_skipped {
        writeln!(
            output,
            "INFO not installed by `cargo xtask install`; binary checks skipped"
        )?;
    }
    writeln!(
        output,
        "Config and grants: current unversioned formats; sessions: format_version {}. Permission session grants are memory-only.",
        tiechui_session::FORMAT_VERSION
    )?;
    let mut result = result;
    match tiechui_tools::exec::resolve_shell() {
        Ok(shell) => writeln!(output, "Shell: {}", shell.display())?,
        Err(error) => result.push(error.to_string()),
    }
    for finding in &result {
        writeln!(output, "FAIL {finding}")?;
    }
    if result.is_empty() {
        writeln!(output, "Doctor: OK")?;
        Ok(())
    } else {
        Err(format!("doctor found {} problem(s)", result.len()).into())
    }
}

fn check(cancel: &CancellationToken) -> Result<()> {
    if cancel.is_cancelled() {
        return Err("doctor cancelled".into());
    }
    Ok(())
}

fn fingerprint(path: &Path, cancel: &CancellationToken) -> Result<String> {
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_file() {
        return Err("expected a regular binary".into());
    }
    let mut file = File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0; 65536];
    loop {
        check(cancel)?;
        let count = file.read(&mut buffer)?;
        if count == 0 {
            return Ok(format!("{:x}", hash.finalize()));
        }
        hash.update(&buffer[..count]);
    }
}

#[cfg_attr(not(unix), allow(unused_variables))]
fn mode(path: &Path, expected: u32, findings: &mut Vec<String>) -> Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() {
        findings.push(format!("{}: symlink is not checked", path.display()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o7777 != expected {
            findings.push(format!(
                "{}: expected permissions {expected:04o}",
                path.display()
            ));
        }
    }
    Ok(())
}

fn json(path: &Path) -> Result<Value> {
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}

fn grants(path: &Path, plugin: bool) -> Result<()> {
    let value = json(path)?;
    if plugin {
        let fields = value.as_object().ok_or("invalid plugin grant")?;
        if fields.len() != 4
            || !value["plugin"].is_string()
            || !value["root"]
                .as_str()
                .is_some_and(|root| Path::new(root).is_absolute())
            || !value["hash"]
                .as_str()
                .and_then(|hash| hash.strip_prefix("sha256:"))
                .is_some_and(|hash| hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()))
            || !value["capabilities"].is_array()
        {
            return Err("unsupported plugin grant schema".into());
        }
        serde_json::from_value::<Vec<tiechui_protocol::PluginCapability>>(
            value["capabilities"].clone(),
        )?;
    } else if path.file_name().is_some_and(|name| name == "trust.json") {
        serde_json::from_value::<BTreeMap<PathBuf, Option<bool>>>(value)?;
    } else {
        serde_json::from_value::<BTreeMap<String, String>>(value)?;
    }
    Ok(())
}

fn walk(
    path: &Path,
    profile: &Path,
    findings: &mut Vec<String>,
    cancel: &CancellationToken,
) -> Result<()> {
    check(cancel)?;
    let meta = fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() {
        findings.push(format!("{}: symlink is not checked", path.display()));
        return Ok(());
    }
    if meta.is_dir() {
        mode(path, 0o700, findings)?;
        for entry in fs::read_dir(path)? {
            let path = entry?.path();
            if let Err(error) = walk(&path, profile, findings, cancel) {
                check(cancel)?;
                findings.push(format!("{}: {error}", path.display()));
            }
        }
        return Ok(());
    }
    if !meta.is_file() {
        return Err("expected a regular profile file".into());
    }
    let relative = path.strip_prefix(profile)?;
    if relative == Path::new("secrets.env")
        || relative.starts_with("mcp-oauth")
        || relative.starts_with("tokens")
    {
        // Only metadata is inspected: credential bytes are never opened.
        mode(path, 0o600, findings)?;
    } else if relative == Path::new("config.toml") {
        let paths = tiechui_config::ConfigPaths {
            user: path.to_owned(),
            project: None,
        };
        // A sibling providers.toml failure is reported by its own entry.
        if tiechui_config::load(&paths, false, &toml::Table::new())
            .is_err_and(|error| error.file == path)
        {
            return Err("invalid config schema or unsupported version".into());
        }
    } else if relative == Path::new("providers.toml") {
        let providers =
            tiechui_config::load_providers(path).map_err(|_| "invalid providers schema")?;
        if providers
            .values()
            .any(|provider| provider.api_key.is_some())
        {
            mode(path, 0o600, findings)?;
        }
    } else if relative.starts_with("sessions") && path.extension().is_some_and(|ext| ext == "jsonl")
    {
        let mut header = String::new();
        BufReader::new(File::open(path)?)
            .take(1024 * 1024)
            .read_line(&mut header)?;
        let header: tiechui_session::Header = serde_json::from_str(&header)?;
        if header.format_version != tiechui_session::FORMAT_VERSION
            || header.id.is_empty()
            || !header.cwd.is_absolute()
        {
            return Err("invalid session header or unsupported format_version".into());
        }
    } else if relative == Path::new("trust.json")
        || relative == Path::new("hooks-trust.json")
        || relative.starts_with("plugin-grants")
            && path.extension().is_some_and(|ext| ext == "json")
    {
        grants(path, relative.starts_with("plugin-grants"))?;
    }
    Ok(())
}

fn inspect(
    prefix: &Path,
    explicit_prefix: bool,
    profile: &Path,
    cancel: &CancellationToken,
) -> Result<(Vec<String>, bool)> {
    let mut findings = Vec::new();
    let state = prefix.join(".tiechui-install");
    if state.join("journal.json").try_exists()? {
        findings.push("interrupted install: rerun xtask install or uninstall to recover".into());
    }
    let ownership = state.join("ownership.json");
    let mut binary_checks_skipped = false;
    let installed = (|| -> Result<()> {
        if matches!(fs::symlink_metadata(&ownership), Err(error) if error.kind() == std::io::ErrorKind::NotFound)
        {
            if explicit_prefix {
                return Err(
                    format!("missing ownership record for prefix {}", prefix.display()).into(),
                );
            }
            binary_checks_skipped = true;
            return Ok(());
        }
        for path in [prefix, &prefix.join("bin"), &state, &ownership] {
            if fs::symlink_metadata(path)?.file_type().is_symlink() {
                return Err("symlink in installation path".into());
            }
        }
        let record = json(&ownership)?;
        if record["schema_version"] != 1 || record["files"].as_object().map(|v| v.len()) != Some(2)
        {
            return Err("unsupported ownership schema".into());
        }
        for name in ["chui", "tiechui"] {
            check(cancel)?;
            let path = prefix.join("bin").join(name);
            match fingerprint(&path, cancel) {
                Ok(hash) if record["files"][name]["sha256"] == hash => {
                    mode(&path, 0o755, &mut findings)?;
                }
                Ok(_) => findings.push(format!("{}: checksum mismatch", path.display())),
                Err(error) => findings.push(format!("{}: {error}", path.display())),
            }
        }
        Ok(())
    })();
    if let Err(error) = installed {
        findings.push(format!("{}: {error}", ownership.display()));
    }
    if let Err(error) = walk(profile, profile, &mut findings, cancel) {
        findings.push(format!("{}: {error}", profile.display()));
    }
    check(cancel)?;
    Ok((findings, binary_checks_skipped))
}
