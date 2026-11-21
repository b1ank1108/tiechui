use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tiechui_protocol::{DEFAULT_RUNTIME, GATED_OPS, RuntimeCapabilities, RuntimeInfo};
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use crate::backend::Backend;

/// How long `<binary> --version` may take.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// What a probe found.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Probe {
    pub available: bool,
    pub version: Option<String>,
    pub path: Option<PathBuf>,
    pub reason: Option<String>,
}

impl Probe {
    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self {
            reason: Some(reason.into()),
            ..Self::default()
        }
    }
}

/// The environment drivers probe and spawn with.
#[derive(Debug, Clone, Default)]
pub struct Launch {
    /// The `PATH` that locates executables, for the probe and for the processes drivers start.
    pub path: Option<OsString>,
}

impl Launch {
    pub fn from_env() -> Self {
        Self {
            path: std::env::var_os("PATH"),
        }
    }
}

/// One external runtime.
#[async_trait]
pub trait Driver: Send + Sync {
    fn id(&self) -> &'static str;
    fn label(&self) -> &'static str;
    fn capabilities(&self) -> RuntimeCapabilities;
    /// The gated ops (names of `GATED_OPS`) the backend implements; they must be exactly the
    /// ones `capabilities` allows.
    fn gated_ops(&self) -> &'static [&'static str];
    /// Whether the runtime can be started, without side effects.
    async fn probe(&self, launch: &Launch) -> Probe;
    fn backend(&self, launch: &Launch) -> Result<Box<dyn Backend>, String>;
}

pub struct Registry {
    drivers: Vec<Arc<dyn Driver>>,
    launch: Launch,
}

impl Registry {
    /// Rejects a driver whose id is `chui` or taken, or whose gated ops and capabilities
    /// disagree.
    pub fn new(drivers: Vec<Arc<dyn Driver>>, launch: Launch) -> Result<Self, String> {
        for (index, driver) in drivers.iter().enumerate() {
            let id = driver.id();
            if id == DEFAULT_RUNTIME {
                return Err(format!("runtime id {id} is the native runtime's"));
            }
            if drivers[..index].iter().any(|other| other.id() == id) {
                return Err(format!("runtime {id} is registered twice"));
            }
            let capabilities = driver.capabilities();
            for (op, feature) in GATED_OPS {
                let implemented = driver.gated_ops().contains(&op);
                if implemented != capabilities.allows(feature) {
                    return Err(format!(
                        "runtime {id}: capabilities {} {op} but the driver {} it",
                        if capabilities.allows(feature) {
                            "allow"
                        } else {
                            "forbid"
                        },
                        if implemented {
                            "implements"
                        } else {
                            "does not implement"
                        },
                    ));
                }
            }
            if let Some(unknown) = driver
                .gated_ops()
                .iter()
                .find(|op| !GATED_OPS.iter().any(|(gated, _)| gated == *op))
            {
                return Err(format!("runtime {id}: {unknown} is not a gated op"));
            }
        }
        Ok(Self { drivers, launch })
    }

    /// The runtimes this build ships: `codex`, and `fake` with the `fake-runtime` feature.
    pub fn builtin(launch: Launch) -> Self {
        Self::new(builtin_drivers(), launch).expect("built-in runtimes are consistent")
    }

    pub fn launch(&self) -> &Launch {
        &self.launch
    }

    pub fn driver(&self, id: &str) -> Option<&Arc<dyn Driver>> {
        self.drivers.iter().find(|driver| driver.id() == id)
    }

    /// The capabilities of a runtime, `chui` included.
    pub fn capabilities(&self, id: &str) -> Option<RuntimeCapabilities> {
        if id == DEFAULT_RUNTIME {
            return Some(RuntimeCapabilities::native());
        }
        self.driver(id).map(|driver| driver.capabilities())
    }

    /// `chui` first and always available, then each driver as probed.
    pub async fn list(&self) -> Vec<RuntimeInfo> {
        let mut infos = vec![RuntimeInfo {
            id: DEFAULT_RUNTIME.into(),
            label: "Tiechui".into(),
            available: true,
            version: Some(env!("CARGO_PKG_VERSION").into()),
            path: None,
            reason: None,
            capabilities: RuntimeCapabilities::native(),
        }];
        for driver in &self.drivers {
            let probe = driver.probe(&self.launch).await;
            infos.push(RuntimeInfo {
                id: driver.id().into(),
                label: driver.label().into(),
                available: probe.available,
                version: probe.version,
                path: probe.path.map(|path| path.display().to_string()),
                reason: probe.reason,
                capabilities: driver.capabilities(),
            });
        }
        infos
    }
}

fn builtin_drivers() -> Vec<Arc<dyn Driver>> {
    #[allow(unused_mut)]
    let mut drivers: Vec<Arc<dyn Driver>> = vec![Arc::new(crate::codex::CodexDriver)];
    #[cfg(feature = "fake-runtime")]
    drivers.push(Arc::new(crate::fake::FakeDriver));
    drivers
}

/// The capabilities of a runtime of this build; the native ones for `chui` and unknown ids.
pub fn capabilities_of(id: &str) -> RuntimeCapabilities {
    builtin_drivers()
        .iter()
        .find(|driver| driver.id() == id)
        .map_or_else(RuntimeCapabilities::native, |driver| driver.capabilities())
}

/// The first executable called `name` in the `PATH` directories.
pub fn resolve_in_path(name: &str, path: Option<&OsStr>) -> Option<PathBuf> {
    let path = path?;
    std::env::split_paths(path)
        .filter(|dir| !dir.as_os_str().is_empty())
        .find_map(|dir| executable_in(&dir, name))
}

#[cfg(unix)]
fn executable_in(dir: &Path, name: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let candidate = dir.join(name);
    let meta = std::fs::metadata(&candidate).ok()?;
    (meta.is_file() && meta.permissions().mode() & 0o111 != 0).then_some(candidate)
}

#[cfg(windows)]
fn executable_in(dir: &Path, name: &str) -> Option<PathBuf> {
    ["exe", "cmd", "bat"].iter().find_map(|extension| {
        let candidate = dir.join(format!("{name}.{extension}"));
        candidate.is_file().then_some(candidate)
    })
}

/// The first dotted number in `text`, such as `0.171.0` of `codex-cli 0.171.0`.
pub fn parse_version(text: &str) -> Option<String> {
    text.split(|c: char| c.is_whitespace() || c == '/' || c == '(' || c == ')')
        .map(|token| token.trim_start_matches('v'))
        .find(|token| {
            token.contains('.')
                && token.starts_with(|c: char| c.is_ascii_digit())
                && token
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '+')
        })
        .map(str::to_owned)
}

/// Locates `binary` on `launch.path` and runs `binary <version_arg>`, which must not touch the
/// runtime's state. A missing binary, a failing or hanging command is unavailable with a reason.
pub async fn probe_binary(binary: &str, version_arg: &str, launch: &Launch) -> Probe {
    let Some(path) = resolve_in_path(binary, launch.path.as_deref()) else {
        return Probe::unavailable(format!("{binary} is not on PATH"));
    };
    let mut command = Command::new(&path);
    command
        .arg(version_arg)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    if let Some(search) = &launch.path {
        command.env("PATH", search);
    }
    let (mut child, tree) = match tiechui_exec::Tree::spawn(&mut command) {
        Ok(spawned) => spawned,
        Err(error) => {
            return Probe {
                path: Some(path),
                reason: Some(format!("cannot run {binary}: {error}")),
                ..Probe::default()
            };
        }
    };
    let Some(mut stdout) = child.stdout.take() else {
        return Probe::unavailable(format!("cannot read {binary} output"));
    };
    let run = async {
        let mut text = String::new();
        let mut buffer = vec![0; 4096];
        // A version line is short; stop reading at 64 KiB in case the binary is not one.
        while text.len() < 64 * 1024 {
            let read = stdout.read(&mut buffer).await?;
            if read == 0 {
                break;
            }
            text.push_str(&String::from_utf8_lossy(&buffer[..read]));
        }
        let status = child.wait().await?;
        Ok::<_, std::io::Error>((status, text))
    };
    let outcome = tokio::time::timeout(PROBE_TIMEOUT, run).await;
    drop(tree);
    match outcome {
        Err(_) => Probe {
            path: Some(path),
            reason: Some(format!(
                "{binary} {version_arg} did not finish in {} s",
                PROBE_TIMEOUT.as_secs()
            )),
            ..Probe::default()
        },
        Ok(Err(error)) => Probe {
            path: Some(path),
            reason: Some(format!("{binary} {version_arg}: {error}")),
            ..Probe::default()
        },
        Ok(Ok((status, _))) if !status.success() => Probe {
            path: Some(path),
            reason: Some(format!("{binary} {version_arg} exited with {status}")),
            ..Probe::default()
        },
        Ok(Ok((_, text))) => match parse_version(&text) {
            Some(version) => Probe {
                available: true,
                version: Some(version),
                path: Some(path),
                reason: None,
            },
            None => Probe {
                path: Some(path),
                reason: Some(format!("{binary} {version_arg} printed no version")),
                ..Probe::default()
            },
        },
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    struct Stub {
        id: &'static str,
        capabilities: RuntimeCapabilities,
        ops: &'static [&'static str],
    }

    #[async_trait]
    impl Driver for Stub {
        fn id(&self) -> &'static str {
            self.id
        }
        fn label(&self) -> &'static str {
            "Stub"
        }
        fn capabilities(&self) -> RuntimeCapabilities {
            self.capabilities
        }
        fn gated_ops(&self) -> &'static [&'static str] {
            self.ops
        }
        async fn probe(&self, _: &Launch) -> Probe {
            Probe::unavailable("stub")
        }
        fn backend(&self, _: &Launch) -> Result<Box<dyn Backend>, String> {
            Err("stub".into())
        }
    }

    fn registry(drivers: Vec<Stub>) -> Result<Registry, String> {
        Registry::new(
            drivers
                .into_iter()
                .map(|driver| Arc::new(driver) as Arc<dyn Driver>)
                .collect(),
            Launch::default(),
        )
    }

    #[test]
    fn capabilities_and_gated_ops_must_agree() {
        let minimal = Stub {
            id: "a",
            capabilities: RuntimeCapabilities::minimal(),
            ops: &[],
        };
        registry(vec![minimal]).unwrap();

        let allows_retry_without_it = Stub {
            id: "a",
            capabilities: RuntimeCapabilities {
                retry: true,
                ..RuntimeCapabilities::minimal()
            },
            ops: &[],
        };
        let error = registry(vec![allows_retry_without_it]).err().unwrap();
        assert!(error.contains("allow retry"), "{error}");

        let implements_compact_without_cap = Stub {
            id: "a",
            capabilities: RuntimeCapabilities::minimal(),
            ops: &["compact"],
        };
        let error = registry(vec![implements_compact_without_cap])
            .err()
            .unwrap();
        assert!(error.contains("forbid compact"), "{error}");

        let commands = Stub {
            id: "a",
            capabilities: RuntimeCapabilities {
                commands: true,
                ..RuntimeCapabilities::minimal()
            },
            ops: &["run_command"],
        };
        assert!(
            registry(vec![commands])
                .err()
                .unwrap()
                .contains("command_input")
        );

        let unknown = Stub {
            id: "a",
            capabilities: RuntimeCapabilities::minimal(),
            ops: &["teleport"],
        };
        assert!(registry(vec![unknown]).err().unwrap().contains("teleport"));
    }

    #[test]
    fn ids_are_unique_and_never_the_native_one() {
        let stub = |id| Stub {
            id,
            capabilities: RuntimeCapabilities::minimal(),
            ops: &[],
        };
        assert!(
            registry(vec![stub("chui")])
                .err()
                .unwrap()
                .contains("native")
        );
        assert!(
            registry(vec![stub("a"), stub("a")])
                .err()
                .unwrap()
                .contains("twice")
        );
    }

    #[test]
    fn builtin_runtimes_are_consistent() {
        let registry = Registry::builtin(Launch::default());
        assert_eq!(
            registry.capabilities("codex"),
            Some(RuntimeCapabilities::minimal())
        );
        assert_eq!(
            registry.capabilities("chui"),
            Some(RuntimeCapabilities::native())
        );
        assert_eq!(registry.capabilities("nope"), None);
    }

    #[test]
    fn versions_are_the_first_dotted_number() {
        assert_eq!(
            parse_version("codex-cli 0.171.0\n").as_deref(),
            Some("0.171.0")
        );
        assert_eq!(
            parse_version("2.1.3 (Claude Code)").as_deref(),
            Some("2.1.3")
        );
        assert_eq!(
            parse_version("agy v1.2.0-beta.1").as_deref(),
            Some("1.2.0-beta.1")
        );
        assert_eq!(parse_version("no version here"), None);
        assert_eq!(parse_version(""), None);
    }

    #[test]
    fn a_missing_path_or_binary_resolves_to_nothing() {
        assert_eq!(resolve_in_path("codex", None), None);
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(resolve_in_path("codex", Some(dir.path().as_os_str())), None);
    }

    #[tokio::test]
    async fn the_listing_leads_with_chui_and_reports_each_driver() {
        let stub = Stub {
            id: "a",
            capabilities: RuntimeCapabilities::minimal(),
            ops: &[],
        };
        let list = registry(vec![stub]).unwrap().list().await;
        assert_eq!(list[0].id, "chui");
        assert!(list[0].available);
        assert_eq!(list[1].id, "a");
        assert!(!list[1].available);
        assert_eq!(list[1].reason.as_deref(), Some("stub"));
    }
}
