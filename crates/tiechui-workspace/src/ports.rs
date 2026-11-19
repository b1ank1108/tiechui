use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};
#[cfg(not(target_os = "linux"))]
use std::process::Command;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use serde_json::json;
use tiechui_protocol::workspace::{PortEntry, PortStopParams, PortsChanged};

use crate::{Result, Sink, fail, lock, root};

const POLL_INTERVAL: Duration = Duration::from_millis(1200);
const RESCAN_DELAY: Duration = Duration::from_millis(500);

#[derive(Clone)]
struct RegisteredRoot {
    raw: String,
    forms: Vec<PathBuf>,
    score: (usize, usize),
}

impl RegisteredRoot {
    fn new(raw: String) -> Result<Self> {
        let path = root(&raw)?;
        let lexical = clean_path(&path);
        let resolved = resolve_existing_prefix(&lexical);
        let score = score_for_path(&resolved);
        let mut forms = vec![resolved];
        if !forms
            .iter()
            .any(|existing| crate::win_path::paths_equal(existing, &lexical))
        {
            forms.push(lexical);
        }
        Ok(Self { raw, forms, score })
    }

    fn matches_path(&self, candidate: &Path) -> bool {
        if self
            .forms
            .iter()
            .any(|f| crate::win_path::looks_like_windows_root(f))
        {
            let cand_norm =
                crate::win_path::normalize_windows_path_key(&candidate.to_string_lossy());
            return self.forms.iter().any(|form| {
                let form_norm =
                    crate::win_path::normalize_windows_path_key(&form.to_string_lossy());
                crate::win_path::windows_path_starts_with(&cand_norm, &form_norm)
            });
        }
        self.forms.iter().any(|form| candidate.starts_with(form))
    }

    fn contains_root(&self, child: &Self) -> bool {
        child.forms.iter().any(|cf| self.matches_path(cf))
    }

    fn same_target(&self, other: &Self) -> bool {
        self.raw == other.raw
            || self.forms.iter().any(|sf| {
                other
                    .forms
                    .iter()
                    .any(|of| crate::win_path::paths_equal(sf, of))
            })
    }
}

fn score_for_path(path: &Path) -> (usize, usize) {
    if crate::win_path::looks_like_windows_root(path) {
        let norm = crate::win_path::normalize_windows_path_key(&path.to_string_lossy());
        let depth = norm.split('\\').filter(|s| !s.is_empty()).count();
        (depth, norm.len())
    } else {
        (path.components().count(), path.as_os_str().len())
    }
}

#[derive(Default)]
struct State {
    client_roots: BTreeMap<u64, Vec<RegisteredRoot>>,
    last: Option<Vec<PortEntry>>,
    rescan_at: Option<Instant>,
    started: bool,
    closed: bool,
}

impl State {
    fn merged_roots(&self) -> Vec<RegisteredRoot> {
        let mut by_raw = BTreeMap::new();
        for roots in self.client_roots.values() {
            for item in roots {
                by_raw
                    .entry(item.raw.clone())
                    .or_insert_with(|| item.clone());
            }
        }
        by_raw.into_values().collect()
    }
}

pub(crate) struct PortMonitor {
    home: Option<PathBuf>,
    sink: Sink,
    state: Mutex<State>,
    wake: Condvar,
}

impl PortMonitor {
    pub(crate) fn new(home: Option<PathBuf>, sink: Sink) -> Arc<Self> {
        Arc::new(Self {
            home,
            sink,
            state: Mutex::default(),
            wake: Condvar::new(),
        })
    }

    pub(crate) fn list(
        self: &Arc<Self>,
        client_id: u64,
        raw_roots: &[String],
    ) -> Result<Vec<PortEntry>> {
        let mut seen = BTreeSet::new();
        let mut caller_roots = Vec::with_capacity(raw_roots.len());
        for item in raw_roots {
            if seen.insert(item.clone()) {
                caller_roots.push(RegisteredRoot::new(item.clone())?);
            }
        }
        let union_roots = {
            let mut state = lock(&self.state);
            if caller_roots.is_empty() {
                state.client_roots.remove(&client_id);
            } else {
                state.client_roots.insert(client_id, caller_roots.clone());
            }
            state.merged_roots()
        };
        let union_ports = if union_roots.is_empty() {
            Vec::new()
        } else {
            scan_attributed(&union_roots, self.home.as_deref())
        };
        let caller_ports = if caller_roots.is_empty() {
            Vec::new()
        } else if same_roots(&caller_roots, &union_roots) {
            union_ports.clone()
        } else {
            scan_attributed(&caller_roots, self.home.as_deref())
        };
        let notify = {
            let mut state = lock(&self.state);
            let current_union = state.merged_roots();
            if !same_roots(&current_union, &union_roots) {
                false
            } else {
                let changed_from_prior = state
                    .last
                    .as_ref()
                    .is_some_and(|previous| previous != &union_ports);
                state.last = Some(union_ports.clone());
                if !current_union.is_empty() && !state.started && !state.closed {
                    state.started = true;
                    spawn_watcher(Arc::clone(self));
                }
                self.wake.notify_all();
                changed_from_prior
            }
        };
        if notify {
            (self.sink)("ports/changed", json!(PortsChanged { ports: union_ports }));
        }
        Ok(caller_ports)
    }

    pub(crate) fn remove_client(&self, client_id: u64) {
        let should_rescan = {
            let mut state = lock(&self.state);
            if state.client_roots.remove(&client_id).is_none() {
                false
            } else if state.client_roots.is_empty() {
                state.last = None;
                state.rescan_at = None;
                self.wake.notify_all();
                false
            } else {
                state.rescan_at = Some(Instant::now());
                self.wake.notify_all();
                true
            }
        };
        if should_rescan {
            self.rescan_now();
        }
    }

    pub(crate) fn stop(self: &Arc<Self>, params: &PortStopParams) -> Result<()> {
        let target_root = RegisteredRoot::new(params.root.clone())?;
        if params.pid <= 1 || params.pid == std::process::id() {
            return fail("refusing to stop server or system process");
        }
        let candidate_roots = {
            let state = lock(&self.state);
            let mut roots = state.merged_roots();
            if !roots.iter().any(|r| r.same_target(&target_root)) {
                roots.push(target_root.clone());
            }
            roots
        };
        let current = scan_attributed(&candidate_roots, self.home.as_deref());
        let owned = current.iter().any(|entry| {
            entry.pid == params.pid
                && (entry.root == params.root
                    || crate::win_path::paths_equal(
                        Path::new(&entry.root),
                        Path::new(&params.root),
                    ))
                && params.port.is_none_or(|port| entry.port == port)
        });
        if !owned {
            return fail(format!(
                "process {} does not own a listening port in {}",
                params.pid, params.root
            ));
        }

        terminate_process(params.pid)?;
        self.rescan_now();
        {
            let mut state = lock(&self.state);
            if !state.client_roots.is_empty() && !state.closed {
                state.rescan_at = Some(Instant::now() + RESCAN_DELAY);
                if !state.started {
                    state.started = true;
                    spawn_watcher(Arc::clone(self));
                }
                self.wake.notify_all();
            }
        }
        Ok(())
    }

    pub(crate) fn close(&self) {
        let mut state = lock(&self.state);
        state.closed = true;
        state.client_roots.clear();
        state.rescan_at = None;
        self.wake.notify_all();
    }

    fn rescan_now(&self) {
        let roots = {
            let state = lock(&self.state);
            let merged = state.merged_roots();
            if state.closed || merged.is_empty() {
                return;
            }
            merged
        };
        self.apply_scan(&roots);
    }

    fn apply_scan(&self, roots: &[RegisteredRoot]) {
        let ports = scan_attributed(roots, self.home.as_deref());
        let emit = {
            let mut state = lock(&self.state);
            let merged = state.merged_roots();
            if state.closed || !same_roots(&merged, roots) {
                false
            } else if state.last.as_ref() != Some(&ports) {
                state.last = Some(ports.clone());
                true
            } else {
                false
            }
        };
        if emit {
            (self.sink)("ports/changed", json!(PortsChanged { ports }));
        }
    }
}

fn same_roots(a: &[RegisteredRoot], b: &[RegisteredRoot]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.raw == y.raw)
}

fn spawn_watcher(monitor: Arc<PortMonitor>) {
    std::thread::spawn(move || {
        loop {
            let roots = {
                let mut state = lock(&monitor.state);
                while !state.closed && state.client_roots.is_empty() {
                    state = monitor
                        .wake
                        .wait(state)
                        .unwrap_or_else(|err| err.into_inner());
                }
                if state.closed {
                    return;
                }
                let now = Instant::now();
                let wait = match state.rescan_at {
                    Some(at) if at <= now => Duration::ZERO,
                    Some(at) => (at - now).min(POLL_INTERVAL),
                    None => POLL_INTERVAL,
                };
                if !wait.is_zero() {
                    let (next, _) = monitor
                        .wake
                        .wait_timeout(state, wait)
                        .unwrap_or_else(|err| err.into_inner());
                    state = next;
                }
                if state.closed {
                    return;
                }
                if state.rescan_at.is_some_and(|at| Instant::now() >= at) {
                    state.rescan_at = None;
                }
                let merged = state.merged_roots();
                if merged.is_empty() {
                    continue;
                }
                merged
            };

            monitor.apply_scan(&roots);
        }
    });
}

pub(crate) fn clean_path(path: &Path) -> PathBuf {
    #[cfg(windows)]
    let stripped = crate::win_path::strip_verbatim_path(path);
    #[cfg(windows)]
    let path = stripped.as_path();
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                let _ = out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

pub(crate) fn resolve_existing_prefix(path: &Path) -> PathBuf {
    #[cfg(windows)]
    let stripped = crate::win_path::strip_verbatim_path(path);
    #[cfg(windows)]
    let path = stripped.as_path();
    if let Ok(canon) = path.canonicalize() {
        return clean_path(&canon);
    }
    for ancestor in path.ancestors().skip(1) {
        if ancestor.as_os_str().is_empty() {
            break;
        }
        if let Ok(canon) = ancestor.canonicalize()
            && let Ok(rest) = path.strip_prefix(ancestor)
        {
            return clean_path(&canon.join(rest));
        }
    }
    clean_path(path)
}

#[derive(Debug)]
struct ListenerProcess {
    pid: u32,
    process: String,
    cwd: Option<PathBuf>,
    cmdline: String,
    args: Vec<String>,
    sockets: Vec<(u16, String)>,
}

fn expand_worktree_roots(roots: &[RegisteredRoot], home: Option<&Path>) -> Vec<RegisteredRoot> {
    let Some(home) = home else {
        return roots.to_vec();
    };
    let owner_raws = owner_raw_map(roots);
    let mut expanded: Vec<RegisteredRoot> = roots
        .iter()
        .map(|item| remap_managed_root(item, home, &owner_raws))
        .collect();
    let mut seen: BTreeSet<PathBuf> = expanded
        .iter()
        .flat_map(|r| r.forms.iter().map(|f| crate::win_path::path_key(f)))
        .collect();
    for item in roots {
        let Ok(project_path) = root(&item.raw) else {
            continue;
        };
        for wt_path in crate::worktree::project_worktree_paths(home, &project_path) {
            if wt_path.is_dir() {
                push_worktree_root(&mut expanded, &mut seen, &item.raw, &wt_path);
            }
        }
    }
    expanded
}

fn owner_raw_map(roots: &[RegisteredRoot]) -> BTreeMap<PathBuf, String> {
    let mut map = BTreeMap::new();
    for item in roots {
        for form in &item.forms {
            map.entry(crate::win_path::path_key(form))
                .or_insert_with(|| item.raw.clone());
        }
    }
    map
}

fn remap_managed_root(
    item: &RegisteredRoot,
    home: &Path,
    owner_raws: &BTreeMap<PathBuf, String>,
) -> RegisteredRoot {
    let Some(owner) = crate::worktree::worktree_project_root(home, Path::new(&item.raw)) else {
        return item.clone();
    };
    let Some(owner_raw) = owner_raws.get(&crate::win_path::path_key(&owner)) else {
        return item.clone();
    };
    let mut remapped = item.clone();
    remapped.raw = owner_raw.clone();
    remapped
}

fn push_worktree_root(
    expanded: &mut Vec<RegisteredRoot>,
    seen: &mut BTreeSet<PathBuf>,
    owner_raw: &str,
    wt_path: &Path,
) {
    let lexical = clean_path(wt_path);
    let resolved = resolve_existing_prefix(&lexical);
    if !seen.insert(crate::win_path::path_key(&resolved)) {
        return;
    }
    let score = score_for_path(&resolved);
    let mut forms = vec![resolved];
    if !forms
        .iter()
        .any(|existing| crate::win_path::paths_equal(existing, &lexical))
    {
        seen.insert(crate::win_path::path_key(&lexical));
        forms.push(lexical);
    }
    expanded.push(RegisteredRoot {
        raw: owner_raw.to_owned(),
        forms,
        score,
    });
}

fn scan_attributed(roots: &[RegisteredRoot], home: Option<&Path>) -> Vec<PortEntry> {
    let roots = expand_worktree_roots(roots, home);
    let listeners = collect_listeners();
    let mut dedup: BTreeMap<(u32, u16), PortEntry> = BTreeMap::new();

    for proc_info in listeners {
        if should_ignore_process(proc_info.pid, &proc_info.process, &proc_info.cmdline) {
            continue;
        }
        let Some(root_idx) = attribute_process(&proc_info, &roots) else {
            continue;
        };
        let root = &roots[root_idx].raw;
        for (port, address) in proc_info.sockets {
            if port == 0 {
                continue;
            }
            dedup
                .entry((proc_info.pid, port))
                .and_modify(|existing| {
                    if prefers_address(&address, &existing.address) {
                        existing.address = address.clone();
                    }
                })
                .or_insert_with(|| PortEntry {
                    root: root.clone(),
                    port,
                    pid: proc_info.pid,
                    process: proc_info.process.clone(),
                    address,
                });
        }
    }

    let mut entries: Vec<PortEntry> = dedup.into_values().collect();
    entries.sort_by(|a, b| (&a.root, a.port, a.pid).cmp(&(&b.root, b.port, b.pid)));
    entries
}

fn prefers_address(candidate: &str, current: &str) -> bool {
    let rank = |addr: &str| -> u8 {
        if !addr.contains(':') && addr != "0.0.0.0" {
            3
        } else if addr == "0.0.0.0" {
            2
        } else if addr != "::" {
            1
        } else {
            0
        }
    };
    rank(candidate) > rank(current)
}

fn should_ignore_process(pid: u32, process: &str, cmdline: &str) -> bool {
    if pid <= 1 || pid == std::process::id() {
        return true;
    }
    let base = Path::new(process)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(process)
        .trim_end_matches(".exe")
        .to_ascii_lowercase();
    if matches!(
        base.as_str(),
        "chui"
            | "tiechui"
            | "tiechui-desktop"
            | "electron"
            | "electron helper"
            | "electron helper (renderer)"
            | "electron helper (gpu)"
            | "electron helper (plugin)"
            | "chromium"
            | "chrome"
            | "google chrome"
            | "google chrome helper"
            | "firefox"
            | "safari"
            | "msedge"
            | "webview2"
            | "agent-browser"
            | "launchd"
            | "systemd"
            | "rapportd"
            | "controlcenter"
            | "ardagent"
            | "sharingd"
            | "mdnsresponder"
            | "sshd"
            | "ssh"
            | "git"
            | "lsof"
            | "ps"
    ) {
        return true;
    }
    cmdline.contains("--remote-debugging-port")
        || cmdline.contains("--type=renderer")
        || cmdline.contains("--type=gpu-process")
        || cmdline.contains("--type=utility")
        || cmdline.contains("--type=zygote")
        || cmdline.contains("--type=extensionHost")
        || cmdline.contains("/.vscode-server/")
        || cmdline.contains("/.cursor-server/")
        || cmdline.contains(r"\.vscode-server\")
        || cmdline.contains(r"\.cursor-server\")
}

fn attribute_process(proc_info: &ListenerProcess, roots: &[RegisteredRoot]) -> Option<usize> {
    let cwd_match = proc_info
        .cwd
        .as_deref()
        .and_then(|cwd| best_root_for_path(cwd, roots));

    let mut cmd_match: Option<usize> = None;
    let mut consider_idx = |idx: usize| {
        if cmd_match.is_none_or(|best| roots[idx].score > roots[best].score) {
            cmd_match = Some(idx);
        }
    };

    for idx in roots_in_cmdline_text(&proc_info.cmdline, roots) {
        consider_idx(idx);
    }
    let win_roots = roots.iter().any(|r| {
        r.forms
            .iter()
            .any(|f| crate::win_path::looks_like_windows_root(f))
    });
    for token in candidate_arg_tokens(&proc_info.args, &proc_info.cmdline, win_roots) {
        if let Some(path) = resolve_token_path(&token, proc_info.cwd.as_deref())
            && let Some(idx) = best_root_for_path(&path, roots)
        {
            consider_idx(idx);
        }
    }

    match (cwd_match, cmd_match) {
        (Some(ci), Some(mi))
            if roots[ci].contains_root(&roots[mi]) && roots[mi].score > roots[ci].score =>
        {
            Some(mi)
        }
        (Some(ci), _) => Some(ci),
        (None, Some(mi)) => Some(mi),
        (None, None) => None,
    }
}

fn best_root_for_path(path: &Path, roots: &[RegisteredRoot]) -> Option<usize> {
    let lexical = clean_path(path);
    let resolved = resolve_existing_prefix(&lexical);
    roots
        .iter()
        .enumerate()
        .filter(|(_, root)| root.matches_path(&resolved) || root.matches_path(&lexical))
        .max_by_key(|(_, root)| root.score)
        .map(|(idx, _)| idx)
}

fn roots_in_cmdline_text(cmdline: &str, roots: &[RegisteredRoot]) -> Vec<usize> {
    if cmdline.is_empty() {
        return Vec::new();
    }
    let has_win = roots.iter().any(|r| {
        r.forms
            .iter()
            .any(|f| crate::win_path::looks_like_windows_root(f))
    });
    let win_cmdline =
        has_win.then(|| crate::win_path::normalize_windows_cmdline_for_search(cmdline));
    let mut matched = Vec::new();
    for (idx, root) in roots.iter().enumerate() {
        let hit = root.forms.iter().any(|form| {
            if let Some(norm_cmd) = &win_cmdline
                && crate::win_path::looks_like_windows_root(form)
            {
                let needle = crate::win_path::normalize_windows_path_key(&form.to_string_lossy());
                return crate::win_path::windows_cmdline_contains_root(norm_cmd, &needle);
            }
            posix_cmdline_contains_root(cmdline, form)
        });
        if hit {
            matched.push(idx);
        }
    }
    matched
}

fn posix_cmdline_contains_root(cmdline: &str, form: &Path) -> bool {
    let Some(needle) = form.to_str().filter(|s| s.len() > 1) else {
        return false;
    };
    let bytes = cmdline.as_bytes();
    let is_left = |pos: usize| -> bool {
        pos == 0 || matches!(bytes[pos - 1], b' ' | b'\t' | b'=' | b'"' | b'\'' | b':')
    };
    let is_right = |end: usize| -> bool {
        end >= bytes.len()
            || matches!(
                bytes[end],
                b'/' | b'\\' | b' ' | b'\t' | b'"' | b'\'' | b':'
            )
    };
    cmdline
        .match_indices(needle)
        .any(|(pos, _)| is_left(pos) && is_right(pos + needle.len()))
}

fn candidate_arg_tokens(args: &[String], cmdline: &str, win_roots: bool) -> Vec<String> {
    let mut out = Vec::new();
    let push_cleaned = |raw: &str, out: &mut Vec<String>| {
        let trimmed = raw.trim_matches(|c| c == '"' || c == '\'');
        if !trimmed.is_empty() {
            out.push(trimmed.to_owned());
        }
        if let Some((_, rhs)) = trimmed.split_once('=') {
            let rhs = rhs.trim_matches(|c| c == '"' || c == '\'');
            if !rhs.is_empty() {
                out.push(rhs.to_owned());
            }
        }
    };
    for arg in args {
        push_cleaned(arg, &mut out);
    }
    if win_roots && args.is_empty() {
        for parsed in crate::win_path::parse_windows_cmdline(cmdline) {
            push_cleaned(&parsed, &mut out);
        }
    }
    for part in cmdline.split_whitespace() {
        push_cleaned(part, &mut out);
    }
    out
}

fn resolve_token_path(token: &str, cwd: Option<&Path>) -> Option<PathBuf> {
    if token.len() <= 1 || token.starts_with('-') || token.contains("://") {
        return None;
    }
    let path = Path::new(token);
    if path.is_absolute() || crate::win_path::looks_like_windows_root(path) {
        return Some(path.to_path_buf());
    }
    if token.contains('/') || (cfg!(windows) && token.contains('\\')) {
        return cwd.map(|base| base.join(path));
    }
    None
}

#[cfg(all(unix, not(target_os = "linux")))]
fn normalize_listen_endpoint(raw: &str) -> Option<(String, u16)> {
    let endpoint = raw.split(" (").next().unwrap_or(raw).trim();
    if endpoint.contains("->") {
        return None;
    }
    let (host, port_str) = endpoint.rsplit_once(':')?;
    let port: u16 = port_str.parse().ok()?;
    if port == 0 {
        return None;
    }
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let address = match host {
        "*" | "" => "0.0.0.0".to_owned(),
        other => other.to_owned(),
    };
    Some((address, port))
}

fn process_display_name(comm: &str, args: &[String], cmdline: &str) -> String {
    let from_arg0 = args
        .first()
        .map(String::as_str)
        .or_else(|| cmdline.split_whitespace().next())
        .and_then(|arg0| Path::new(arg0).file_name())
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty());
    let chosen = if !comm.is_empty() {
        Path::new(comm)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(comm)
    } else {
        from_arg0.unwrap_or("unknown")
    };
    strip_exe_suffix(chosen).to_owned()
}

fn strip_exe_suffix(name: &str) -> &str {
    if name.len() > 4 && name[name.len() - 4..].eq_ignore_ascii_case(".exe") {
        &name[..name.len() - 4]
    } else {
        name
    }
}

#[cfg(target_os = "linux")]
fn collect_listeners() -> Vec<ListenerProcess> {
    use std::collections::HashMap;
    use std::net::{Ipv4Addr, Ipv6Addr};

    let mut inode_sockets: HashMap<u64, (u16, String)> = HashMap::new();
    for (file, ipv6) in [("/proc/self/net/tcp", false), ("/proc/self/net/tcp6", true)] {
        let Ok(content) = std::fs::read_to_string(file) else {
            continue;
        };
        for line in content.lines().skip(1) {
            let cols: Vec<&str> = line.split_whitespace().collect();
            if cols.len() < 10 || cols[3] != "0A" {
                continue;
            }
            let Some((ip_hex, port_hex)) = cols[1].split_once(':') else {
                continue;
            };
            let Ok(port) = u16::from_str_radix(port_hex, 16) else {
                continue;
            };
            let Ok(inode) = cols[9].parse::<u64>() else {
                continue;
            };
            if port == 0 || inode == 0 {
                continue;
            }
            let address = if !ipv6 {
                let Ok(raw) = u32::from_str_radix(ip_hex, 16) else {
                    continue;
                };
                Ipv4Addr::from(raw.to_ne_bytes()).to_string()
            } else {
                if ip_hex.len() != 32 {
                    continue;
                }
                let mut bytes = [0u8; 16];
                let mut ok = true;
                for i in 0..4 {
                    match u32::from_str_radix(&ip_hex[i * 8..(i + 1) * 8], 16) {
                        Ok(word) => bytes[i * 4..(i + 1) * 4].copy_from_slice(&word.to_ne_bytes()),
                        Err(_) => {
                            ok = false;
                            break;
                        }
                    }
                }
                if !ok {
                    continue;
                }
                let v6 = Ipv6Addr::from(bytes);
                match v6.to_ipv4_mapped() {
                    Some(v4) => v4.to_string(),
                    None => v6.to_string(),
                }
            };
            inode_sockets.insert(inode, (port, address));
        }
    }

    if inode_sockets.is_empty() {
        return Vec::new();
    }

    let Ok(proc_dir) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let self_pid = std::process::id();
    let mut out = Vec::new();

    for entry in proc_dir.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        if pid <= 1 || pid == self_pid {
            continue;
        }
        let fd_dir = format!("/proc/{pid}/fd");
        let Ok(fds) = std::fs::read_dir(&fd_dir) else {
            continue;
        };
        let mut sockets = Vec::new();
        for fd in fds.flatten() {
            let Ok(target) = std::fs::read_link(fd.path()) else {
                continue;
            };
            let Some(s) = target.to_str() else {
                continue;
            };
            let inode = s
                .strip_prefix("socket:[")
                .and_then(|r| r.strip_suffix(']'))
                .and_then(|n| n.parse::<u64>().ok());
            if let Some(inode) = inode
                && let Some(sock) = inode_sockets.get(&inode)
            {
                sockets.push(sock.clone());
            }
        }
        if sockets.is_empty() {
            continue;
        }

        let cwd = std::fs::read_link(format!("/proc/{pid}/cwd")).ok();
        let args: Vec<String> = std::fs::read(format!("/proc/{pid}/cmdline"))
            .ok()
            .map(|bytes| {
                bytes
                    .split(|&b| b == 0)
                    .filter(|part| !part.is_empty())
                    .map(|part| String::from_utf8_lossy(part).into_owned())
                    .collect()
            })
            .unwrap_or_default();
        let cmdline = args.join(" ");
        let comm = std::fs::read_to_string(format!("/proc/{pid}/comm"))
            .unwrap_or_default()
            .trim()
            .to_owned();
        let process =
            process_display_name(if args.is_empty() { &comm } else { "" }, &args, &cmdline);
        let process = if process == "unknown" && !comm.is_empty() {
            comm
        } else {
            process
        };
        out.push(ListenerProcess {
            pid,
            process,
            cwd,
            cmdline,
            args,
            sockets,
        });
    }
    out
}

#[cfg(all(unix, not(target_os = "linux")))]
fn find_unix_tool<'a>(fallback: &'a str, candidates: &[&'a str]) -> &'a str {
    candidates
        .iter()
        .copied()
        .find(|p| Path::new(p).exists())
        .unwrap_or(fallback)
}

#[cfg(all(unix, not(target_os = "linux")))]
fn collect_listeners() -> Vec<ListenerProcess> {
    use std::collections::HashMap;

    let lsof = find_unix_tool("lsof", &["/usr/sbin/lsof", "/usr/bin/lsof", "/bin/lsof"]);
    let ps = find_unix_tool("ps", &["/bin/ps", "/usr/bin/ps"]);

    let Ok(output) = Command::new(lsof)
        .args(["+c", "0", "-nP", "-iTCP", "-sTCP:LISTEN", "-F0pcn"])
        .output()
    else {
        return Vec::new();
    };

    let self_pid = std::process::id();
    let mut by_pid: BTreeMap<u32, (String, Vec<(u16, String)>)> = BTreeMap::new();
    let mut current_pid: Option<u32> = None;

    for line in output.stdout.split(|&b| b == b'\n') {
        for field in line.split(|&b| b == 0) {
            if field.is_empty() {
                continue;
            }
            let tag = field[0];
            let body = String::from_utf8_lossy(&field[1..]);
            match tag {
                b'p' => {
                    current_pid = body
                        .parse::<u32>()
                        .ok()
                        .filter(|&pid| pid > 1 && pid != self_pid);
                    if let Some(pid) = current_pid {
                        by_pid.entry(pid).or_default();
                    }
                }
                b'c' => {
                    if let Some(pid) = current_pid
                        && let Some(entry) = by_pid.get_mut(&pid)
                    {
                        entry.0 = body.into_owned();
                    }
                }
                b'n' => {
                    if let Some(pid) = current_pid
                        && let Some((addr, port)) = normalize_listen_endpoint(&body)
                        && let Some(entry) = by_pid.get_mut(&pid)
                    {
                        entry.1.push((port, addr));
                    }
                }
                _ => {}
            }
        }
    }

    by_pid.retain(|_, (_, sockets)| !sockets.is_empty());
    if by_pid.is_empty() {
        return Vec::new();
    }

    let pid_csv = by_pid
        .keys()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let mut cwds: HashMap<u32, PathBuf> = HashMap::new();
    if let Ok(cwd_out) = Command::new(lsof)
        .args(["-a", "-d", "cwd", "-p", &pid_csv, "-F0pn"])
        .output()
    {
        let mut pid: Option<u32> = None;
        for line in cwd_out.stdout.split(|&b| b == b'\n') {
            for field in line.split(|&b| b == 0) {
                if field.is_empty() {
                    continue;
                }
                match field[0] {
                    b'p' => {
                        pid = std::str::from_utf8(&field[1..])
                            .ok()
                            .and_then(|s| s.parse().ok())
                    }
                    b'n' => {
                        if let Some(p) = pid {
                            cwds.insert(
                                p,
                                PathBuf::from(String::from_utf8_lossy(&field[1..]).into_owned()),
                            );
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    let mut cmdlines: HashMap<u32, String> = HashMap::new();
    if let Ok(ps_out) = Command::new(ps)
        .args(["-axww", "-o", "pid=,args="])
        .output()
    {
        for line in String::from_utf8_lossy(&ps_out.stdout).lines() {
            let trimmed = line.trim_start();
            let Some((pid_str, rest)) = trimmed.split_once(char::is_whitespace) else {
                continue;
            };
            if let Ok(pid) = pid_str.parse::<u32>()
                && by_pid.contains_key(&pid)
            {
                cmdlines.insert(pid, rest.trim_start().to_owned());
            }
        }
    }

    by_pid
        .into_iter()
        .map(|(pid, (comm, sockets))| {
            let cmdline = cmdlines.remove(&pid).unwrap_or_default();
            let process = process_display_name(&comm, &[], &cmdline);
            ListenerProcess {
                pid,
                process,
                cwd: cwds.remove(&pid),
                cmdline,
                args: Vec::new(),
                sockets,
            }
        })
        .collect()
}

#[cfg(windows)]
fn collect_listeners() -> Vec<ListenerProcess> {
    win_listeners::collect()
}

#[cfg(windows)]
mod win_listeners {
    use std::collections::{BTreeMap, HashMap};
    use std::ffi::OsString;
    use std::net::{Ipv4Addr, Ipv6Addr};
    use std::os::windows::ffi::OsStringExt;
    use std::sync::{Mutex, OnceLock};

    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_INSUFFICIENT_BUFFER, FILETIME, GetLastError, HANDLE, NO_ERROR,
    };
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        GetExtendedTcpTable, MIB_TCP_STATE_LISTEN, MIB_TCP6ROW_OWNER_PID, MIB_TCP6TABLE_OWNER_PID,
        MIB_TCPROW_OWNER_PID, MIB_TCPTABLE_OWNER_PID, TCP_TABLE_OWNER_PID_LISTENER,
    };
    use windows_sys::Win32::Networking::WinSock::{AF_INET, AF_INET6};
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
    };

    use super::{ListenerProcess, lock, process_display_name};

    const PROCESS_COMMAND_LINE_INFORMATION: i32 = 60;
    const MAX_CMDLINE_BYTES: u32 = 1024 * 1024;

    #[link(name = "ntdll")]
    unsafe extern "system" {
        fn NtQueryInformationProcess(
            process_handle: HANDLE,
            process_information_class: i32,
            process_information: *mut core::ffi::c_void,
            process_information_length: u32,
            return_length: *mut u32,
        ) -> i32;
    }

    #[repr(C)]
    struct UnicodeString {
        length: u16,
        maximum_length: u16,
        buffer: *mut u16,
    }

    struct WinHandle(HANDLE);

    impl Drop for WinHandle {
        fn drop(&mut self) {
            // SAFETY: `self.0` was opened via `OpenProcess` and is owned exclusively here.
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }

    #[derive(Clone)]
    struct CachedProcess {
        creation_time: u64,
        image_path: String,
        cmdline: String,
        args: Vec<String>,
    }

    fn process_cache() -> &'static Mutex<HashMap<u32, CachedProcess>> {
        static CACHE: OnceLock<Mutex<HashMap<u32, CachedProcess>>> = OnceLock::new();
        CACHE.get_or_init(Mutex::default)
    }

    pub(super) fn collect() -> Vec<ListenerProcess> {
        let self_pid = std::process::id();
        let mut by_pid: BTreeMap<u32, Vec<(u16, String)>> = BTreeMap::new();
        collect_ipv4(self_pid, &mut by_pid);
        collect_ipv6(self_pid, &mut by_pid);
        prune_cache(&by_pid);
        if by_pid.is_empty() {
            return Vec::new();
        }
        by_pid
            .into_iter()
            .map(|(pid, sockets)| {
                let (image_path, cmdline, args) = resolve_process_meta(pid);
                let process = process_display_name(&image_path, &args, &cmdline);
                ListenerProcess {
                    pid,
                    process,
                    cwd: None,
                    cmdline,
                    args,
                    sockets,
                }
            })
            .collect()
    }

    fn prune_cache(active: &BTreeMap<u32, Vec<(u16, String)>>) {
        lock(process_cache()).retain(|pid, _| active.contains_key(pid));
    }

    fn fetch_tcp_table(af: u32) -> Option<(Vec<u32>, usize)> {
        let mut size: u32 = 0;
        // SAFETY: querying required byte count with a null buffer is documented by IpHlpApi.
        let status = unsafe {
            GetExtendedTcpTable(
                std::ptr::null_mut(),
                &mut size,
                0,
                af,
                TCP_TABLE_OWNER_PID_LISTENER,
                0,
            )
        };
        if status != ERROR_INSUFFICIENT_BUFFER && status != NO_ERROR {
            return None;
        }
        for _ in 0..4 {
            let words = (size as usize).div_ceil(4).max(1);
            let mut buf = vec![0_u32; words];
            let mut actual = (words * 4) as u32;
            // SAFETY: `buf` is 4-byte aligned and holds `actual` writable bytes.
            let rc = unsafe {
                GetExtendedTcpTable(
                    buf.as_mut_ptr().cast(),
                    &mut actual,
                    0,
                    af,
                    TCP_TABLE_OWNER_PID_LISTENER,
                    0,
                )
            };
            if rc == NO_ERROR {
                return Some((buf, (actual as usize).min(words * 4)));
            }
            if rc != ERROR_INSUFFICIENT_BUFFER {
                return None;
            }
            size = actual.saturating_add(256);
        }
        None
    }

    fn collect_ipv4(self_pid: u32, by_pid: &mut BTreeMap<u32, Vec<(u16, String)>>) {
        let Some((buf, byte_len)) = fetch_tcp_table(u32::from(AF_INET)) else {
            return;
        };
        let rows_offset = std::mem::offset_of!(MIB_TCPTABLE_OWNER_PID, table);
        if byte_len < rows_offset {
            return;
        }
        let max_rows = (byte_len - rows_offset) / std::mem::size_of::<MIB_TCPROW_OWNER_PID>();
        let count = (buf[0] as usize).min(max_rows);
        // SAFETY: `rows_offset + count * sizeof(row) <= byte_len` and alignment is 4 bytes.
        let rows = unsafe {
            std::slice::from_raw_parts(
                buf.as_ptr()
                    .byte_add(rows_offset)
                    .cast::<MIB_TCPROW_OWNER_PID>(),
                count,
            )
        };
        for row in rows {
            if row.dwState != MIB_TCP_STATE_LISTEN as u32 {
                continue;
            }
            let pid = row.dwOwningPid;
            let port = u16::from_be(row.dwLocalPort as u16);
            if pid <= 4 || pid == self_pid || port == 0 {
                continue;
            }
            let addr = Ipv4Addr::from(row.dwLocalAddr.to_ne_bytes()).to_string();
            by_pid.entry(pid).or_default().push((port, addr));
        }
    }

    fn collect_ipv6(self_pid: u32, by_pid: &mut BTreeMap<u32, Vec<(u16, String)>>) {
        let Some((buf, byte_len)) = fetch_tcp_table(u32::from(AF_INET6)) else {
            return;
        };
        let rows_offset = std::mem::offset_of!(MIB_TCP6TABLE_OWNER_PID, table);
        if byte_len < rows_offset {
            return;
        }
        let max_rows = (byte_len - rows_offset) / std::mem::size_of::<MIB_TCP6ROW_OWNER_PID>();
        let count = (buf[0] as usize).min(max_rows);
        // SAFETY: `rows_offset + count * sizeof(row) <= byte_len` and alignment is 4 bytes.
        let rows = unsafe {
            std::slice::from_raw_parts(
                buf.as_ptr()
                    .byte_add(rows_offset)
                    .cast::<MIB_TCP6ROW_OWNER_PID>(),
                count,
            )
        };
        for row in rows {
            if row.dwState != MIB_TCP_STATE_LISTEN as u32 {
                continue;
            }
            let pid = row.dwOwningPid;
            let port = u16::from_be(row.dwLocalPort as u16);
            if pid <= 4 || pid == self_pid || port == 0 {
                continue;
            }
            let v6 = Ipv6Addr::from(row.ucLocalAddr);
            let addr = match v6.to_ipv4_mapped() {
                Some(v4) => v4.to_string(),
                None => v6.to_string(),
            };
            by_pid.entry(pid).or_default().push((port, addr));
        }
    }

    fn resolve_process_meta(pid: u32) -> (String, String, Vec<String>) {
        // SAFETY: `OpenProcess` with `PROCESS_QUERY_LIMITED_INFORMATION` does not touch target VM.
        let raw = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if raw.is_null() {
            return (String::new(), String::new(), Vec::new());
        }
        let handle = WinHandle(raw);
        let creation_time = query_creation_time(handle.0);
        if let Some(ct) = creation_time
            && let Some(hit) = lock(process_cache())
                .get(&pid)
                .filter(|entry| entry.creation_time == ct)
                .cloned()
        {
            return (hit.image_path, hit.cmdline, hit.args);
        }
        let image_path = query_image_path(handle.0).unwrap_or_default();
        let cmdline = query_command_line(handle.0).unwrap_or_default();
        let mut args = crate::win_path::parse_windows_cmdline(&cmdline);
        if !image_path.is_empty() && !args.iter().any(|a| a == &image_path) {
            args.insert(0, image_path.clone());
        }
        if let Some(ct) = creation_time {
            lock(process_cache()).insert(
                pid,
                CachedProcess {
                    creation_time: ct,
                    image_path: image_path.clone(),
                    cmdline: cmdline.clone(),
                    args: args.clone(),
                },
            );
        }
        (image_path, cmdline, args)
    }

    fn query_creation_time(handle: HANDLE) -> Option<u64> {
        let mut created = FILETIME::default();
        let mut exited = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        // SAFETY: `handle` is valid and all `FILETIME` pointers are writable stack locals.
        let ok =
            unsafe { GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user) };
        (ok != 0)
            .then(|| (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
    }

    fn query_image_path(handle: HANDLE) -> Option<String> {
        let mut cap: u32 = 512;
        while cap <= 32_768 {
            let mut buf = vec![0_u16; cap as usize];
            let mut len = cap;
            // SAFETY: `buf` has `len` writable `u16` elements.
            let ok = unsafe { QueryFullProcessImageNameW(handle, 0, buf.as_mut_ptr(), &mut len) };
            if ok != 0 {
                let raw = OsString::from_wide(&buf[..len as usize])
                    .to_string_lossy()
                    .into_owned();
                return Some(crate::win_path::strip_windows_verbatim_str(&raw));
            }
            // SAFETY: reads thread-local Win32 error code immediately after failed call.
            if unsafe { GetLastError() } != ERROR_INSUFFICIENT_BUFFER {
                return None;
            }
            cap *= 2;
        }
        None
    }

    fn query_command_line(handle: HANDLE) -> Option<String> {
        let mut needed: u32 = 1024;
        for _ in 0..3 {
            let words = (needed as usize).div_ceil(8).max(4);
            let byte_cap = words * 8;
            let mut buf = vec![0_u64; words];
            let mut ret_len: u32 = 0;
            // SAFETY: `buf` is 8-byte aligned and holds `byte_cap` writable bytes; queries kernel
            // `ProcessCommandLineInformation` (60) without reading target PEB memory.
            let status = unsafe {
                NtQueryInformationProcess(
                    handle,
                    PROCESS_COMMAND_LINE_INFORMATION,
                    buf.as_mut_ptr().cast(),
                    byte_cap as u32,
                    &mut ret_len,
                )
            };
            if status >= 0 {
                return decode_unicode_string(&buf, byte_cap);
            }
            if ret_len <= byte_cap as u32 || ret_len > MAX_CMDLINE_BYTES {
                return None;
            }
            needed = ret_len.saturating_add(64);
        }
        None
    }

    fn decode_unicode_string(buf: &[u64], byte_cap: usize) -> Option<String> {
        if byte_cap < std::mem::size_of::<UnicodeString>() {
            return None;
        }
        // SAFETY: `buf` is 8-byte aligned and at least `size_of::<UnicodeString>()` bytes long.
        let ustr = unsafe { std::ptr::read(buf.as_ptr().cast::<UnicodeString>()) };
        if ustr.length == 0 {
            return Some(String::new());
        }
        let base = buf.as_ptr() as usize;
        let ptr = ustr.buffer as usize;
        let len_bytes = usize::from(ustr.length);
        if ustr.buffer.is_null()
            || !ptr.is_multiple_of(std::mem::align_of::<u16>())
            || ptr < base
            || ptr.saturating_add(len_bytes) > base.saturating_add(byte_cap)
        {
            return None;
        }
        // SAFETY: bounds and `u16` alignment within `buf` were verified above.
        let wide = unsafe { std::slice::from_raw_parts(ustr.buffer, len_bytes / 2) };
        Some(String::from_utf16_lossy(wide))
    }
}

#[cfg(unix)]
const SIGTERM: i32 = 15;
#[cfg(unix)]
const SIGKILL: i32 = 9;

#[cfg(unix)]
fn terminate_process(pid: u32) -> Result<()> {
    unsafe extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }

    let raw_pid = i32::try_from(pid).map_err(|_| "invalid pid")?;
    if raw_pid <= 1 {
        return fail("refusing to signal system process");
    }

    if unsafe { kill(raw_pid, SIGTERM) } != 0 {
        let err = std::io::Error::last_os_error();
        if err.kind() == std::io::ErrorKind::PermissionDenied
            || err.kind() == std::io::ErrorKind::InvalidInput
        {
            return Err(err.into());
        }
        return Ok(());
    }

    let grace_deadline = Instant::now() + Duration::from_millis(300);
    while Instant::now() < grace_deadline {
        if !unix_process_running(raw_pid) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    if unsafe { kill(raw_pid, SIGKILL) } != 0 {
        let err = std::io::Error::last_os_error();
        if err.kind() == std::io::ErrorKind::PermissionDenied {
            return Err(err.into());
        }
        return Ok(());
    }

    let kill_deadline = Instant::now() + Duration::from_millis(150);
    while Instant::now() < kill_deadline {
        if !unix_process_running(raw_pid) {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

#[cfg(unix)]
fn unix_process_running(pid: i32) -> bool {
    unsafe extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }

    if unsafe { kill(pid, 0) } != 0 {
        return std::io::Error::last_os_error().kind() == std::io::ErrorKind::PermissionDenied;
    }

    #[cfg(target_os = "linux")]
    {
        if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat"))
            && let Some((_, after)) = stat.rsplit_once(')')
            && let Some(state) = after.split_whitespace().next()
        {
            return !matches!(state, "Z" | "X" | "x");
        }
    }

    #[cfg(not(target_os = "linux"))]
    {
        let ps = find_unix_tool("ps", &["/bin/ps", "/usr/bin/ps"]);
        if let Ok(out) = Command::new(ps)
            .args(["-o", "state=", "-p", &pid.to_string()])
            .output()
        {
            let state = String::from_utf8_lossy(&out.stdout);
            let trimmed = state.trim();
            if trimmed.is_empty() || trimmed.starts_with('Z') {
                return false;
            }
        }
    }

    true
}

#[cfg(windows)]
fn terminate_process(pid: u32) -> Result<()> {
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;

    let out = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(std::process::Stdio::null())
        .output()?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        if !stderr.is_empty() {
            return fail(stderr.trim().to_owned());
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn fake_win_root(raw: &str) -> RegisteredRoot {
        let norm = crate::win_path::normalize_windows_path_key(raw);
        let depth = norm.split('\\').filter(|s| !s.is_empty()).count();
        RegisteredRoot {
            raw: raw.to_owned(),
            forms: vec![PathBuf::from(raw)],
            score: (depth, norm.len()),
        }
    }

    #[test]
    fn windows_attribution_matches_verbatim_mixed_case_forward_slashes_and_longest_prefix() {
        let roots = vec![
            fake_win_root(r"C:\Users\Alice\My Repo"),
            fake_win_root(r"C:\Users\Alice\My Repo\packages\web"),
            fake_win_root(r"C:\Users\Alice\My Repo-extra"),
        ];
        let proc_nested = ListenerProcess {
            pid: 4200,
            process: "node".into(),
            cwd: None,
            cmdline: r#""C:\Program Files\nodejs\node.exe" "\\?\c:/users/alice/my repo/packages/web/vite.js" --port 5173"#.into(),
            args: crate::win_path::parse_windows_cmdline(
                r#""C:\Program Files\nodejs\node.exe" "\\?\c:/users/alice/my repo/packages/web/vite.js" --port 5173"#,
            ),
            sockets: vec![(5173, "127.0.0.1".into())],
        };
        assert_eq!(
            roots_in_cmdline_text(&proc_nested.cmdline, &roots),
            vec![0, 1]
        );
        assert_eq!(attribute_process(&proc_nested, &roots), Some(1));

        let proc_sibling = ListenerProcess {
            pid: 4201,
            process: "node".into(),
            cwd: None,
            cmdline: r#"node.exe c:\users\alice\my repo-extra\server.js"#.into(),
            args: vec![],
            sockets: vec![(3000, "127.0.0.1".into())],
        };
        assert_eq!(
            roots_in_cmdline_text(&proc_sibling.cmdline, &roots),
            vec![2]
        );
        assert_eq!(attribute_process(&proc_sibling, &roots), Some(2));

        let proc_parent_dotdot = ListenerProcess {
            pid: 4202,
            process: "node".into(),
            cwd: None,
            cmdline: r#"node.exe --script="c:\users\alice\my repo\packages\web\..\..\server.js""#
                .into(),
            args: vec![],
            sockets: vec![(8080, "127.0.0.1".into())],
        };
        assert_eq!(attribute_process(&proc_parent_dotdot, &roots), Some(0));
    }
}
