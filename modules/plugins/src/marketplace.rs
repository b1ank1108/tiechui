//! Git snapshots and Codex marketplace manifests. No package code runs on install.
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tiechui_plugin_api::{CancellationToken, ToolError, process::ProcessGroup, serde_json};
use tokio::io::AsyncReadExt;

use crate::{check, manifest, store};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Marketplace {
    pub name: String,
    pub source: String,
    pub reference: Option<String>,
    pub commit: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Pin {
    pub name: String,
    pub marketplace_commit: String,
    pub source: String,
    pub commit: String,
}

#[derive(Deserialize)]
struct Catalog {
    name: String,
    plugins: Vec<Entry>,
}

#[derive(Deserialize)]
struct Entry {
    name: String,
    source: Source,
    #[serde(flatten)]
    manifest: serde_json::Map<String, serde_json::Value>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Source {
    Path(String),
    Object(GitSource),
    Unsupported(serde_json::Value),
}

#[derive(Deserialize)]
#[serde(tag = "source", rename_all = "lowercase")]
enum GitSource {
    Local {
        path: String,
    },
    Url {
        url: String,
        path: Option<String>,
        #[serde(rename = "ref")]
        reference: Option<String>,
        sha: Option<String>,
    },
    #[serde(rename = "git-subdir")]
    GitSubdir {
        url: String,
        path: String,
        #[serde(rename = "ref")]
        reference: Option<String>,
        sha: Option<String>,
    },
}

fn valid_name(name: &str) -> Result<(), ToolError> {
    if !name.is_empty()
        && !name.starts_with('.')
        && !name.ends_with('.')
        && !name.contains("..")
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    {
        Ok(())
    } else {
        Err(ToolError("invalid marketplace name".into()))
    }
}

fn catalog(root: &Path) -> Result<Catalog, ToolError> {
    let path = [
        ".agents/plugins/marketplace.json",
        ".agents/plugins/api_marketplace.json",
        ".claude-plugin/marketplace.json",
        ".cursor-plugin/marketplace.json",
        "marketplace.json",
    ]
    .into_iter()
    .map(|p| root.join(p))
    .find(|p| p.exists())
    .ok_or_else(|| ToolError("marketplace.json not found".into()))?;
    if !path.canonicalize()?.starts_with(root.canonicalize()?) {
        return Err(ToolError("marketplace manifest escapes repository".into()));
    }
    let catalog: Catalog =
        serde_json::from_slice(&fs::read(path)?).map_err(|e| ToolError(e.to_string()))?;
    valid_name(&catalog.name)?;
    let mut names = BTreeSet::new();
    for entry in &catalog.plugins {
        if !manifest::valid_name(&entry.name) || !names.insert(&entry.name) {
            return Err(ToolError(
                "invalid or duplicate marketplace plugin name".into(),
            ));
        }
    }
    Ok(catalog)
}

async fn git(cwd: &Path, args: &[String], cancel: &CancellationToken) -> Result<String, ToolError> {
    check(cancel)?;
    let mut command = vec!["-c".into(), "core.hooksPath=/dev/null".into()];
    command.extend_from_slice(args);
    let mut process = ProcessGroup::spawn(
        Path::new("git"),
        &command,
        cwd,
        &[],
        &BTreeMap::from([
            ("GIT_TERMINAL_PROMPT".into(), "0".into()),
            ("GIT_OPTIONAL_LOCKS".into(), "0".into()),
            ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
            ("GIT_CONFIG_GLOBAL".into(), "/dev/null".into()),
            ("HOME".into(), cwd.display().to_string()),
            (
                "XDG_CONFIG_HOME".into(),
                cwd.join(".config").display().to_string(),
            ),
        ]),
    )?;
    process.child.stdin.take();
    let mut stdout = process
        .child
        .stdout
        .take()
        .ok_or_else(|| ToolError("missing git stdout".into()))?;
    let operation = async {
        let mut output = String::new();
        stdout.read_to_string(&mut output).await?;
        if !process.child.wait().await?.success() {
            return Err(ToolError(format!(
                "git {} failed",
                args.first().map_or("", String::as_str)
            )));
        }
        Ok(output.trim().to_owned())
    };
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(ToolError("Operation aborted".into())),
        result = tokio::time::timeout(Duration::from_secs(120), operation) =>
            result.map_err(|_| ToolError("git operation timed out".into()))?,
    }
}

fn clone_source(source: &str) -> String {
    #[cfg(windows)]
    if let Some(path) = source.strip_prefix(r"\\?\UNC\") {
        return format!("//{}", path.replace('\\', "/"));
    } else if let Some(path) = source.strip_prefix(r"\\?\") {
        return path.replace('\\', "/");
    }
    source.into()
}

#[cfg(all(test, windows))]
#[test]
fn clone_sources_preserve_urls_and_normalize_verbatim_paths() {
    for (source, expected) in [
        (r"\\?\E:\market place\repo", "E:/market place/repo"),
        (r"\\?\UNC\server\share\repo", "//server/share/repo"),
        (
            "https://example.invalid/repo.git",
            "https://example.invalid/repo.git",
        ),
        (
            "git@example.invalid:repo.git",
            "git@example.invalid:repo.git",
        ),
    ] {
        assert_eq!(clone_source(source), expected);
    }
}

async fn checkout(
    source: &str,
    reference: Option<&str>,
    root: &Path,
    cancel: &CancellationToken,
) -> Result<String, ToolError> {
    git(
        root,
        &[
            "clone".into(),
            "--no-checkout".into(),
            "--no-hardlinks".into(),
            "--".into(),
            clone_source(source),
            "repo".into(),
        ],
        cancel,
    )
    .await?;
    let repo = root.join("repo");
    let revision = format!("{}^{{commit}}", reference.unwrap_or("HEAD"));
    let commit = git(
        &repo,
        &[
            "rev-parse".into(),
            "--verify".into(),
            "--end-of-options".into(),
            revision,
        ],
        cancel,
    )
    .await?;
    git(
        &repo,
        &[
            "checkout".into(),
            "--detach".into(),
            commit.clone(),
            "--".into(),
        ],
        cancel,
    )
    .await?;
    Ok(commit)
}

fn stage(home: &Path) -> Result<tempfile::TempDir, ToolError> {
    tiechui_plugin_api::private::create_dir_all(&home.join("marketplaces"))?;
    Ok(tempfile::Builder::new()
        .prefix(".stage-")
        .tempdir_in(home.join("marketplaces"))?)
}

fn save(home: &Path, market: &Marketplace) -> Result<(), ToolError> {
    let mut file = tempfile::NamedTempFile::new_in(home.join("marketplaces"))?;
    serde_json::to_writer_pretty(&mut file, market).map_err(|e| ToolError(e.to_string()))?;
    file.as_file().sync_all()?;
    file.persist(
        home.join("marketplaces")
            .join(format!("{}.json", market.name)),
    )
    .map_err(|e| ToolError(e.to_string()))?;
    Ok(())
}

pub async fn add(
    source: &str,
    reference: Option<&str>,
    cwd: &Path,
    home: &Path,
    cancel: &CancellationToken,
) -> Result<Marketplace, ToolError> {
    let source = if cwd.join(source).exists() {
        cwd.join(source).canonicalize()?.display().to_string()
    } else {
        source.into()
    };
    let stage = stage(home)?;
    let commit = checkout(&source, reference, stage.path(), cancel).await?;
    let catalog = catalog(&stage.path().join("repo"))?;
    let market = Marketplace {
        name: catalog.name,
        source,
        reference: reference.map(str::to_owned),
        commit,
    };
    let _lock = store::lock(home, cancel)?;
    if list(home, cancel)?.iter().any(|m| m.name == market.name) {
        return Err(ToolError(
            "marketplace already added; remove it before replacing its source".into(),
        ));
    }
    save(home, &market)?;
    Ok(market)
}

pub fn list(home: &Path, cancel: &CancellationToken) -> Result<Vec<Marketplace>, ToolError> {
    let mut result = Vec::new();
    let directory = home.join("marketplaces");
    if !directory.exists() {
        return Ok(result);
    }
    for entry in fs::read_dir(directory)? {
        check(cancel)?;
        let path = entry?.path();
        if path.extension().is_some_and(|s| s == "json") {
            result.push(
                serde_json::from_slice::<Marketplace>(&fs::read(path)?)
                    .map_err(|e| ToolError(e.to_string()))?,
            );
        }
    }
    result.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(result)
}

pub fn remove(
    name_or_source: &str,
    cwd: &Path,
    home: &Path,
    cancel: &CancellationToken,
) -> Result<String, ToolError> {
    let _lock = store::lock(home, cancel)?;
    let local = cwd.join(name_or_source).canonicalize().ok();
    let market = list(home, cancel)?
        .into_iter()
        .find(|m| {
            m.name == name_or_source
                || m.source == name_or_source
                || local.as_deref() == Some(Path::new(&m.source))
        })
        .ok_or_else(|| ToolError("marketplace not found".into()))?;
    valid_name(&market.name)?;
    fs::remove_file(
        home.join("marketplaces")
            .join(format!("{}.json", market.name)),
    )?;
    Ok(market.name)
}

fn package_path(root: &Path, path: &str) -> Result<PathBuf, ToolError> {
    let relative = Path::new(path);
    if relative.is_absolute()
        || relative
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::Prefix(_)))
    {
        return Err(ToolError(
            "plugin path must stay inside its repository".into(),
        ));
    }
    let path = root.join(relative).canonicalize()?;
    if !path.starts_with(root.canonicalize()?) {
        return Err(ToolError("plugin path escapes repository".into()));
    }
    Ok(path)
}

#[derive(Debug, Serialize)]
pub struct Upgrade {
    pub name: String,
    pub old_commit: String,
    pub new_commit: String,
    pub old_hash: String,
    pub new_hash: String,
    pub has_capabilities: bool,
    pub changes: Vec<String>,
}

fn files(root: &Path, cancel: &CancellationToken) -> Result<BTreeMap<PathBuf, String>, ToolError> {
    let mut result = BTreeMap::new();
    store::walk(root, root, cancel, false, &mut |path, directory| {
        if !directory {
            let mut hash = Sha256::new();
            hash.update(fs::read(root.join(path))?);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                hash.update(
                    (fs::metadata(root.join(path))?.permissions().mode() & 0o111).to_le_bytes(),
                );
            }
            result.insert(path.to_owned(), format!("{:x}", hash.finalize()));
        }
        Ok(())
    })?;
    Ok(result)
}

pub async fn install(
    spec: &str,
    home: &Path,
    upgrade: bool,
    cancel: &CancellationToken,
) -> Result<Upgrade, ToolError> {
    let (name, market_name) = spec
        .rsplit_once('@')
        .ok_or_else(|| ToolError("expected name@marketplace".into()))?;
    if !manifest::valid_name(name) {
        return Err(ToolError("invalid plugin name".into()));
    }
    let market = list(home, cancel)?
        .into_iter()
        .find(|m| m.name == market_name)
        .ok_or_else(|| ToolError("marketplace not found".into()))?;
    let snapshot = stage(home)?;
    let market_commit = checkout(
        &market.source,
        if upgrade {
            market.reference.as_deref()
        } else {
            Some(&market.commit)
        },
        snapshot.path(),
        cancel,
    )
    .await?;
    let repo = snapshot.path().join("repo");
    let catalog = catalog(&repo)?;
    if catalog.name != market_name {
        return Err(ToolError("marketplace name changed".into()));
    }
    let entry = catalog
        .plugins
        .into_iter()
        .find(|e| e.name == name)
        .ok_or_else(|| ToolError("plugin not found in marketplace".into()))?;
    let external = stage(home)?;
    let (root, source, commit) = match entry.source {
        Source::Unsupported(source) => {
            return Err(ToolError(format!(
                "unsupported marketplace plugin source: {}",
                source
                    .get("source")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("unknown")
            )));
        }
        Source::Path(path) | Source::Object(GitSource::Local { path }) => (
            package_path(&repo, &path)?,
            market.source,
            market_commit.clone(),
        ),
        Source::Object(source) => {
            let (url, path, reference, sha) = match source {
                GitSource::Url {
                    url,
                    path,
                    reference,
                    sha,
                } => (url, path.unwrap_or_else(|| ".".into()), reference, sha),
                GitSource::GitSubdir {
                    url,
                    path,
                    reference,
                    sha,
                } => (url, path, reference, sha),
                GitSource::Local { .. } => unreachable!(),
            };
            let commit = checkout(
                &url,
                sha.as_deref().or(reference.as_deref()),
                external.path(),
                cancel,
            )
            .await?;
            (
                package_path(&external.path().join("repo"), &path)?,
                url,
                commit,
            )
        }
    };
    let package = stage(home)?;
    store::copy(&root, package.path(), cancel)?;
    let root = package.path().to_owned();
    if ![
        ".tiechui-plugin/plugin.json",
        ".codex-plugin/plugin.json",
        "plugin.json",
    ]
    .iter()
    .any(|path| root.join(path).exists())
    {
        let mut manifest = entry.manifest;
        manifest.insert("name".into(), serde_json::Value::String(name.into()));
        fs::create_dir_all(root.join(".codex-plugin"))?;
        fs::write(
            root.join(".codex-plugin/plugin.json"),
            serde_json::to_vec(&manifest).map_err(|e| ToolError(e.to_string()))?,
        )?;
    }
    let plugin = manifest::load(&root, false, None, cancel)?;
    if plugin.name != name {
        return Err(ToolError("marketplace and plugin names differ".into()));
    }
    let pin = Pin {
        name: market_name.into(),
        marketplace_commit: market_commit,
        source,
        commit: commit.clone(),
    };
    let previous = if upgrade {
        Some(
            store::read_installation(home, name)?
                .ok_or_else(|| ToolError("plugin is not installed".into()))?,
        )
    } else {
        None
    };
    let destination = home.join("plugins").join(name);
    let new_files = files(&root, cancel)?;
    let old_files = if upgrade {
        files(&destination, cancel)?
    } else {
        BTreeMap::new()
    };
    let changes = old_files
        .keys()
        .chain(new_files.keys())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter_map(|path| {
            let old = old_files.get(path);
            let new = new_files.get(path);
            (old != new).then(|| {
                format!(
                    "{} {}",
                    if old.is_none() {
                        "A"
                    } else if new.is_none() {
                        "D"
                    } else {
                        "M"
                    },
                    path.display()
                )
            })
        })
        .collect();
    let old_hash = if upgrade {
        crate::content_hash(&destination, cancel)?
    } else {
        String::new()
    };
    let new_hash = crate::content_hash(&root, cancel)?;
    let old_commit = previous
        .and_then(|p| p.marketplace)
        .map_or(String::new(), |p| p.commit);
    store::install(
        &root,
        home,
        Some(pin),
        upgrade.then_some(old_hash.as_str()),
        cancel,
    )?;
    Ok(Upgrade {
        name: name.into(),
        old_commit,
        new_commit: commit,
        old_hash,
        new_hash,
        has_capabilities: plugin
            .extension
            .is_some_and(|extension| !extension.capabilities.is_empty()),
        changes,
    })
}

pub async fn upgrade(
    name: Option<&str>,
    home: &Path,
    cancel: &CancellationToken,
) -> Result<Vec<Upgrade>, ToolError> {
    let mut specs = Vec::new();
    let records = home.join("plugins/.records");
    if records.exists() {
        for entry in fs::read_dir(records)? {
            check(cancel)?;
            let path = entry?.path();
            if let Some(plugin) = path.file_stem().and_then(|s| s.to_str())
                && name.is_none_or(|name| name == plugin)
                && let Some(record) = store::read_installation(home, plugin)?
                && let Some(pin) = record.marketplace
            {
                specs.push(format!("{plugin}@{}", pin.name));
            }
        }
    }
    if name.is_some() && specs.is_empty() {
        return Err(ToolError("plugin has no marketplace installation".into()));
    }
    specs.sort();
    let mut result = Vec::new();
    for spec in specs {
        result.push(install(&spec, home, true, cancel).await?);
    }
    Ok(result)
}
