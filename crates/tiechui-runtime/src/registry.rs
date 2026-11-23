use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tiechui_protocol::{DEFAULT_RUNTIME, GATED_OPS, RuntimeCapabilities, RuntimeInfo};
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use crate::backend::Backend;
use crate::process::seconds;

/// How long `<binary> --version` may take.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
pub(crate) const MAX_CLI_OUTPUT: usize = 64 * 1024;

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

    pub(crate) fn found_at(path: PathBuf, reason: String) -> Self {
        Self {
            path: Some(path),
            reason: Some(reason),
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
            check_driver_capabilities(id, driver.as_ref())?;
        }
        Ok(Self { drivers, launch })
    }

    /// The runtimes this build ships: `codex`, `claude`, `agy`, and `fake` with the
    /// `fake-runtime` feature.
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

fn check_driver_capabilities(id: &str, driver: &dyn Driver) -> Result<(), String> {
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
    Ok(())
}

fn builtin_drivers() -> Vec<Arc<dyn Driver>> {
    #[allow(unused_mut)]
    let mut drivers: Vec<Arc<dyn Driver>> = vec![
        Arc::new(crate::codex::CodexDriver),
        Arc::new(crate::claude::ClaudeDriver),
        Arc::new(crate::agy::AgyDriver),
    ];
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

/// `probe_binary` with `--version`, unavailable when the version is older than `minimum`.
pub async fn probe_at_least(binary: &str, minimum: &str, launch: &Launch) -> Probe {
    let mut probe = probe_binary(binary, "--version", launch).await;
    if let (true, Some(version)) = (probe.available, &probe.version)
        && let Some(reason) = too_old(binary, version, minimum)
    {
        probe.reason = Some(reason);
        probe.available = false;
    }
    probe
}

/// Why `binary` cannot run at `version`: it is older than `minimum` (as dotted numbers).
pub fn too_old(binary: &str, version: &str, minimum: &str) -> Option<String> {
    (!at_least(version, minimum)).then(|| {
        format!("{binary} {version} is older than the minimum supported version {minimum}")
    })
}

/// Whether dotted `version` is at least `minimum`.
pub fn at_least(version: &str, minimum: &str) -> bool {
    let (mut version, mut minimum) = (version_parts(version), version_parts(minimum));
    let len = version.len().max(minimum.len());
    version.resize(len, 0);
    minimum.resize(len, 0);
    version >= minimum
}

fn version_parts(version: &str) -> Vec<u64> {
    version
        .split('.')
        .map(|part| {
            part.chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>()
                .parse()
                .unwrap_or(0)
        })
        .collect()
}

/// Locates `binary` on `launch.path` and runs `binary <version_arg>`, which must not touch the
/// runtime's state. A missing binary, a failing or hanging command is unavailable with a reason.
pub async fn probe_binary(binary: &str, version_arg: &str, launch: &Launch) -> Probe {
    let Some(path) = resolve_in_path(binary, launch.path.as_deref()) else {
        return Probe::unavailable(format!("{binary} is not on PATH"));
    };
    let mut command = Command::new(&path);
    command.arg(version_arg);
    if let Some(search) = &launch.path {
        command.env("PATH", search);
    }
    let command_line = format!("{binary} {version_arg}");
    match run_bounded_cli(command, binary, &command_line, PROBE_TIMEOUT).await {
        Err(reason) => Probe::found_at(path, reason),
        Ok(text) => match parse_version(&text) {
            Some(version) => Probe {
                available: true,
                version: Some(version),
                path: Some(path),
                reason: None,
            },
            None => Probe::found_at(path, format!("{command_line} printed no version")),
        },
    }
}

pub(crate) async fn run_bounded_cli(
    mut command: Command,
    binary: &str,
    command_line: &str,
    timeout: Duration,
) -> Result<String, String> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let (mut child, tree) = tiechui_exec::Tree::spawn(&mut command)
        .map_err(|error| format!("cannot run {binary}: {error}"))?;
    let Some(stdout) = child.stdout.take() else {
        let _ = tree.terminate(&mut child).await;
        let _ = child.wait().await;
        return Err(format!("cannot read {binary} output"));
    };
    let outcome = tokio::time::timeout(timeout, read_bounded_stdout(stdout, &mut child)).await;
    match outcome {
        Err(_) => {
            let _ = tree.terminate(&mut child).await;
            let _ = child.wait().await;
            Err(format!(
                "{command_line} did not finish in {}",
                seconds(timeout)
            ))
        }
        Ok(Err(error)) => {
            let _ = tree.terminate(&mut child).await;
            let _ = child.wait().await;
            Err(format!("{command_line}: {error}"))
        }
        Ok(Ok((status, _))) if !status.success() => {
            Err(format!("{command_line} exited with {status}"))
        }
        Ok(Ok((_, text))) => Ok(text),
    }
}

async fn read_bounded_stdout(
    mut stdout: tokio::process::ChildStdout,
    child: &mut tokio::process::Child,
) -> std::io::Result<(std::process::ExitStatus, String)> {
    let mut bytes = Vec::new();
    let mut buffer = [0u8; 4096];
    while bytes.len() < MAX_CLI_OUTPUT {
        let limit = (MAX_CLI_OUTPUT - bytes.len()).min(buffer.len());
        let read = stdout.read(&mut buffer[..limit]).await?;
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..read]);
    }
    drop(stdout);
    let status = child.wait().await?;
    Ok((status, String::from_utf8_lossy(&bytes).into_owned()))
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
    fn each_capability_requires_exactly_its_own_ops() {
        let stub = |capabilities, ops| Stub {
            id: "a",
            capabilities,
            ops,
        };
        let with =
            |capabilities: RuntimeCapabilities, ops| registry(vec![stub(capabilities, ops)]).err();
        let permissions = RuntimeCapabilities {
            permissions: true,
            ..RuntimeCapabilities::minimal()
        };
        assert_eq!(with(permissions, &["permissions"]), None);
        let error = with(permissions, &[]).unwrap();
        assert!(error.contains("allow permissions"), "{error}");
        let error = with(RuntimeCapabilities::minimal(), &["permissions"]).unwrap();
        assert!(error.contains("forbid permissions"), "{error}");
        let error = with(permissions, &["run_command", "permissions"]).unwrap();
        assert!(error.contains("forbid run_command"), "{error}");

        let commands = RuntimeCapabilities {
            commands: true,
            ..RuntimeCapabilities::minimal()
        };
        let error = with(commands, &["run_command", "command_input", "permissions"]).unwrap();
        assert!(error.contains("forbid permissions"), "{error}");
        assert_eq!(with(commands, &["run_command", "command_input"]), None);

        let thinking = RuntimeCapabilities {
            thinking: true,
            ..RuntimeCapabilities::minimal()
        };
        assert_eq!(with(thinking, &["set_thinking"]), None);
        assert!(with(thinking, &[]).unwrap().contains("allow set_thinking"));
        let model = RuntimeCapabilities {
            model: true,
            ..RuntimeCapabilities::minimal()
        };
        assert_eq!(with(model, &["set_model"]), None);
        assert!(with(model, &[]).unwrap().contains("allow set_model"));
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
            Some(RuntimeCapabilities {
                commands: true,
                permissions: true,
                thinking: true,
                model: true,
                ..RuntimeCapabilities::minimal()
            })
        );
        assert_eq!(
            registry.capabilities("claude"),
            Some(RuntimeCapabilities {
                steering: tiechui_protocol::SteeringMode::Inject,
                ..RuntimeCapabilities::minimal()
            })
        );
        assert_eq!(
            registry.capabilities("agy"),
            Some(RuntimeCapabilities {
                steering: tiechui_protocol::SteeringMode::Queue,
                commands: true,
                permissions: true,
                model: true,
                ..RuntimeCapabilities::minimal()
            })
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
    fn versions_compare_as_dotted_numbers() {
        assert!(at_least("0.171.0", "0.156.1"));
        assert!(at_least("0.156.1", "0.156.1"));
        assert!(at_least("1.0.0", "0.156.1"));
        assert!(at_least("0.156.1-alpha.2", "0.156.1"));
        assert!(at_least("0.200", "0.156.1"));
        assert!(!at_least("0.156.0", "0.156.1"));
        assert!(!at_least("0.99.9", "0.156.1"));
        assert!(!at_least("0.156", "0.156.1"));
        assert!(at_least("2.1.322", "2.1.322"));
        assert!(at_least("2.1.400", "2.1.322"));
        assert!(at_least("2.2.0", "2.1.322"));
        assert!(at_least("3.0.0", "2.1.322"));
        assert!(!at_least("2.1.321", "2.1.322"));
        assert!(!at_least("2.0.999", "2.1.322"));
        assert!(!at_least("1.9.9", "2.1.322"));
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

    #[cfg(unix)]
    #[tokio::test]
    async fn bounded_cli_handles_split_utf8_large_output_and_reaps_timed_out_children() {
        let mut utf8_cmd = Command::new("node");
        utf8_cmd.args([
            "-e",
            "const prefix = 'a'.repeat(4094); process.stdout.write(prefix + '中文字节');",
        ]);
        let decoded = run_bounded_cli(utf8_cmd, "node", "node utf8", Duration::from_secs(5))
            .await
            .unwrap();
        assert!(
            decoded.ends_with("中文字节"),
            "split UTF-8 must stay intact"
        );
        assert!(!decoded.contains('\u{fffd}'));

        let mut flood_cmd = Command::new("node");
        flood_cmd.args([
            "-e",
            "process.stdout.on('error', () => process.exit(0)); process.stdout.write('x'.repeat(512 * 1024));",
        ]);
        let truncated = run_bounded_cli(flood_cmd, "node", "node flood", Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(truncated.len(), MAX_CLI_OUTPUT);

        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("hang.pid");
        let mut hang_cmd = Command::new("/bin/sh");
        hang_cmd.args([
            "-c",
            &format!("echo $$ > \"{}\"; exec sleep 60", pid_file.display()),
        ]);
        let err = run_bounded_cli(hang_cmd, "sh", "sh hang", Duration::from_millis(200))
            .await
            .unwrap_err();
        assert!(err.contains("did not finish"), "{err}");
        let pid: i32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        for _ in 0..40 {
            // SAFETY: signal 0 only checks whether the process still exists.
            if unsafe { libc::kill(pid, 0) != 0 } {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("timed-out CLI child {pid} was not reaped");
    }
}
