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
        let score = (resolved.components().count(), resolved.as_os_str().len());
        let mut forms = vec![resolved];
        if !forms.contains(&lexical) {
            forms.push(lexical);
        }
        Ok(Self { raw, forms, score })
    }

    fn matches_path(&self, candidate: &Path) -> bool {
        self.forms.iter().any(|form| candidate.starts_with(form))
    }

    fn contains_root(&self, child: &Self) -> bool {
        child
            .forms
            .iter()
            .any(|cf| self.forms.iter().any(|sf| cf.starts_with(sf)))
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
    sink: Sink,
    state: Mutex<State>,
    wake: Condvar,
}

impl PortMonitor {
    pub(crate) fn new(sink: Sink) -> Arc<Self> {
        Arc::new(Self {
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
            scan_attributed(&union_roots)
        };
        let caller_ports = if caller_roots.is_empty() {
            Vec::new()
        } else if same_roots(&caller_roots, &union_roots) {
            union_ports.clone()
        } else {
            scan_attributed(&caller_roots)
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
            if !roots.iter().any(|r| r.raw == target_root.raw) {
                roots.push(target_root);
            }
            roots
        };
        let current = scan_attributed(&candidate_roots);
        let owned = current.iter().any(|entry| {
            entry.pid == params.pid
                && entry.root == params.root
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
        let ports = scan_attributed(roots);
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

fn clean_path(path: &Path) -> PathBuf {
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

fn resolve_existing_prefix(path: &Path) -> PathBuf {
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

fn scan_attributed(roots: &[RegisteredRoot]) -> Vec<PortEntry> {
    let listeners = collect_listeners();
    let mut dedup: BTreeMap<(u32, u16), PortEntry> = BTreeMap::new();

    for proc_info in listeners {
        if should_ignore_process(proc_info.pid, &proc_info.process, &proc_info.cmdline) {
            continue;
        }
        let Some(root_idx) = attribute_process(&proc_info, roots) else {
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
    for token in candidate_arg_tokens(&proc_info.args, &proc_info.cmdline) {
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
    let bytes = cmdline.as_bytes();
    let is_left_boundary = |pos: usize| -> bool {
        pos == 0 || matches!(bytes[pos - 1], b' ' | b'\t' | b'=' | b'"' | b'\'' | b':')
    };
    let is_right_boundary = |end: usize| -> bool {
        end >= bytes.len()
            || matches!(
                bytes[end],
                b'/' | b'\\' | b' ' | b'\t' | b'"' | b'\'' | b':'
            )
    };

    let mut matched = Vec::new();
    for (idx, root) in roots.iter().enumerate() {
        let hit = root.forms.iter().any(|form| {
            let Some(needle) = form.to_str().filter(|s| s.len() > 1) else {
                return false;
            };
            cmdline
                .match_indices(needle)
                .any(|(pos, _)| is_left_boundary(pos) && is_right_boundary(pos + needle.len()))
        });
        if hit {
            matched.push(idx);
        }
    }
    matched
}

fn candidate_arg_tokens(args: &[String], cmdline: &str) -> Vec<String> {
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
    if path.is_absolute() {
        return Some(path.to_path_buf());
    }
    if token.contains('/') || (cfg!(windows) && token.contains('\\')) {
        return cwd.map(|base| base.join(path));
    }
    None
}

#[cfg(not(target_os = "linux"))]
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
    chosen.trim_end_matches(".exe").to_owned()
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
    let mut by_pid: BTreeMap<u32, Vec<(u16, String)>> = BTreeMap::new();
    let self_pid = std::process::id();
    for proto in ["TCP", "TCPv6"] {
        let Ok(out) = Command::new("netstat").args(["-ano", "-p", proto]).output() else {
            continue;
        };
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            let cols: Vec<&str> = line.split_whitespace().collect();
            if cols.len() < 5 || !cols[3].eq_ignore_ascii_case("LISTENING") {
                continue;
            }
            let Ok(pid) = cols[4].parse::<u32>() else {
                continue;
            };
            if pid <= 4 || pid == self_pid {
                continue;
            }
            if let Some((addr, port)) = normalize_listen_endpoint(cols[1]) {
                by_pid.entry(pid).or_default().push((port, addr));
            }
        }
    }
    if by_pid.is_empty() {
        return Vec::new();
    }

    let filter = by_pid
        .keys()
        .map(|pid| format!("ProcessId={pid}"))
        .collect::<Vec<_>>()
        .join(" OR ");
    let script = format!(
        "Get-CimInstance Win32_Process -Filter '{filter}' | Select-Object ProcessId,Name,CommandLine,ExecutablePath | ConvertTo-Json -Compress"
    );
    let mut details: BTreeMap<u32, (String, String, Vec<String>)> = BTreeMap::new();
    if let Ok(out) = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .output()
        && let Ok(val) = serde_json::from_slice::<serde_json::Value>(&out.stdout)
    {
        let items = match val {
            serde_json::Value::Array(arr) => arr,
            obj @ serde_json::Value::Object(_) => vec![obj],
            _ => Vec::new(),
        };
        for item in items {
            let Some(pid) = item["ProcessId"].as_u64().map(|n| n as u32) else {
                continue;
            };
            let name = item["Name"].as_str().unwrap_or_default().to_owned();
            let cmd = item["CommandLine"].as_str().unwrap_or_default().to_owned();
            let mut args = Vec::new();
            if let Some(exe) = item["ExecutablePath"].as_str().filter(|s| !s.is_empty()) {
                args.push(exe.to_owned());
            }
            details.insert(pid, (name, cmd, args));
        }
    }

    by_pid
        .into_iter()
        .map(|(pid, sockets)| {
            let (name, cmdline, args) = details.remove(&pid).unwrap_or_default();
            let process = process_display_name(&name, &args, &cmdline);
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
    let out = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .output()?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        if !stderr.is_empty() {
            return fail(stderr.trim().to_owned());
        }
    }
    Ok(())
}
