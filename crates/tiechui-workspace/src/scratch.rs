use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use crate::ports::{clean_path, resolve_existing_prefix};
use crate::worktree::{now_ms, random_short_id, utc_ymd};
use crate::{Result, fail};

const SCRATCH_FALLBACK_SUMMARY: &str = "scratch";
const MAX_SUMMARY_CHARS: usize = 24;

const WINDOWS_RESERVED: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM0", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7",
    "COM8", "COM9", "LPT0", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

pub fn sanitize_scratch_summary(raw: Option<&str>) -> String {
    let collected = collect_sanitized_chars(raw.unwrap_or_default());
    let trimmed = collected.trim_end_matches(['-', '_', '.']);
    if trimmed.is_empty() || is_windows_reserved(trimmed) {
        SCRATCH_FALLBACK_SUMMARY.to_owned()
    } else {
        trimmed.to_owned()
    }
}

struct SanitizerState {
    out: String,
    count: usize,
    pending_dash: bool,
}

impl SanitizerState {
    fn step(&mut self, ch: char) -> bool {
        if ch.is_alphanumeric() {
            return self.push_alnum(ch);
        }
        if self.count == 0 {
            return true;
        }
        if ch == '_' && !self.pending_dash {
            return self.push_single('_');
        }
        self.pending_dash = true;
        true
    }

    fn push_alnum(&mut self, ch: char) -> bool {
        if self.pending_dash {
            if self.count + 2 > MAX_SUMMARY_CHARS {
                return false;
            }
            self.out.push('-');
            self.out.push(ch);
            self.count += 2;
            self.pending_dash = false;
            return true;
        }
        self.push_single(ch)
    }

    fn push_single(&mut self, ch: char) -> bool {
        if self.count >= MAX_SUMMARY_CHARS {
            return false;
        }
        self.out.push(ch);
        self.count += 1;
        true
    }
}

fn collect_sanitized_chars(raw: &str) -> String {
    let mut state = SanitizerState {
        out: String::new(),
        count: 0,
        pending_dash: false,
    };
    for ch in raw.chars() {
        if !state.step(ch) {
            break;
        }
    }
    state.out
}

fn is_windows_reserved(name: &str) -> bool {
    WINDOWS_RESERVED
        .iter()
        .any(|reserved| name.eq_ignore_ascii_case(reserved))
}

fn scratch_root(home: &Path) -> Result<PathBuf> {
    let abs = if home.is_absolute() {
        home.to_path_buf()
    } else {
        std::path::absolute(home)?
    };
    Ok(clean_path(&abs).join("scratch"))
}

pub fn allocate_scratch_dir(home: &Path, summary: Option<&str>) -> Result<PathBuf> {
    allocate_scratch_dir_with_generator(home, summary, now_ms(), random_short_id)
}

fn allocate_scratch_dir_with_generator(
    home: &Path,
    summary: Option<&str>,
    now_millis: u64,
    mut next_id: impl FnMut(u32) -> String,
) -> Result<PathBuf> {
    let root = scratch_root(home)?;
    if let Some(parent) = root.parent() {
        tiechui_config::private::create_dir_all(parent)?;
    }
    tiechui_config::private::create_dir_all(&root)?;
    let (year, month, day) = utc_ymd(now_millis);
    let slug = sanitize_scratch_summary(summary);
    for attempt in 0..64_u32 {
        let short_id = next_id(attempt);
        let candidate = root.join(format!("{year:04}-{month:02}-{day:02}-{slug}-{short_id}"));
        if try_create_private_dir(&candidate)? {
            return Ok(candidate);
        }
    }
    fail("failed to allocate a unique scratch directory")
}

fn try_create_private_dir(path: &Path) -> std::io::Result<bool> {
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    match builder.create(path) {
        Ok(()) => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
            }
            Ok(true)
        }
        Err(err) if err.kind() == ErrorKind::AlreadyExists => Ok(false),
        Err(err) => Err(err),
    }
}

pub fn list_scratch_dirs(home: &Path) -> Result<Vec<PathBuf>> {
    let root = scratch_root(home)?;
    let entries = match fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err.into()),
    };
    let mut dirs = Vec::new();
    for entry in entries {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            dirs.push(entry.path());
        }
    }
    dirs.sort();
    Ok(dirs)
}

pub fn is_scratch_dir(home: &Path, cwd: &Path) -> bool {
    let Ok(root) = scratch_root(home) else {
        return false;
    };
    let root_resolved = resolve_existing_prefix(&root);
    let cwd_resolved = resolve_existing_prefix(&clean_path(cwd));
    cwd_resolved
        .strip_prefix(&root_resolved)
        .is_ok_and(|rel| !rel.as_os_str().is_empty())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::fs;
    use std::path::Path;

    use super::{
        MAX_SUMMARY_CHARS, SCRATCH_FALLBACK_SUMMARY, allocate_scratch_dir,
        allocate_scratch_dir_with_generator, is_scratch_dir, list_scratch_dirs,
        sanitize_scratch_summary,
    };

    #[test]
    fn sanitizes_cjk_ascii_separators_and_truncates_on_char_boundary() {
        assert_eq!(
            sanitize_scratch_summary(Some("  帮我写个 Python 脚本：解析 CSV！\n第二行  ")),
            "帮我写个-Python-脚本-解析-CSV-第二"
        );
        assert_eq!(
            sanitize_scratch_summary(Some("___fix_login-timeout___")),
            "fix_login-timeout"
        );
        let long_cjk = "测试中文字符边界截断不会劈开任何多字节汉字一二三四五六七八九十";
        let sanitized = sanitize_scratch_summary(Some(long_cjk));
        assert_eq!(sanitized.chars().count(), MAX_SUMMARY_CHARS);
        assert!(long_cjk.starts_with(&sanitized));
    }

    #[test]
    fn falls_back_to_scratch_on_empty_punctuation_traversal_and_reserved_names() {
        for raw in [
            None,
            Some(""),
            Some("   \t\n  "),
            Some("...---___!@#$%^&*()"),
            Some("../../../"),
            Some("CON"),
            Some("con"),
            Some("Prn"),
            Some("AUX"),
            Some("nul"),
            Some("COM1"),
            Some("com9"),
            Some("LPT1"),
            Some("lpt9"),
        ] {
            assert_eq!(
                sanitize_scratch_summary(raw),
                SCRATCH_FALLBACK_SUMMARY,
                "input {raw:?}"
            );
        }
        assert_eq!(
            sanitize_scratch_summary(Some("../../etc/passwd")),
            "etc-passwd"
        );
    }

    #[test]
    fn allocates_unique_private_scratch_directories_and_retries_collisions() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");

        let first = allocate_scratch_dir(&home, Some("整理日志")).unwrap();
        let second = allocate_scratch_dir(&home, Some("整理日志")).unwrap();
        assert_ne!(first, second);
        assert!(first.is_dir());
        assert!(second.is_dir());
        assert_eq!(first.parent(), Some(home.join("scratch").as_path()));

        let first_name = first.file_name().unwrap().to_str().unwrap();
        let parts: Vec<&str> = first_name.split('-').collect();
        assert_eq!(parts.len(), 5, "unexpected name: {first_name}");
        assert_eq!(parts[0].len(), 4);
        assert_eq!(parts[1].len(), 2);
        assert_eq!(parts[2].len(), 2);
        assert_eq!(parts[3], "整理日志");
        assert_eq!(parts[4].len(), 8);
        assert!(
            parts[4]
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let scratch_mode = fs::metadata(home.join("scratch"))
                .unwrap()
                .permissions()
                .mode()
                & 0o7777;
            let child_mode = fs::metadata(&first).unwrap().permissions().mode() & 0o7777;
            assert_eq!(scratch_mode, 0o700);
            assert_eq!(child_mode, 0o700);
        }

        let fixed_ms = 1_794_614_400_000_u64;
        let colliding = home.join("scratch/2026-11-14-demo-aaaa1111");
        fs::create_dir(&colliding).unwrap();
        let mut attempts = 0_u32;
        let retried =
            allocate_scratch_dir_with_generator(&home, Some("demo"), fixed_ms, |attempt| {
                attempts += 1;
                if attempt == 0 {
                    "aaaa1111".into()
                } else {
                    "bbbb2222".into()
                }
            })
            .unwrap();
        assert_eq!(attempts, 2);
        assert_eq!(retried, home.join("scratch/2026-11-14-demo-bbbb2222"));
        assert!(retried.is_dir());
    }

    #[test]
    fn list_scratch_dirs_skips_missing_root_without_creating_it_and_matches_children() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        fs::create_dir_all(&home).unwrap();

        let empty = list_scratch_dirs(&home).unwrap();
        assert!(empty.is_empty());
        assert!(!home.join("scratch").exists());
        assert!(!is_scratch_dir(&home, Path::new("/tmp/other")));
        assert!(!is_scratch_dir(&home, &home.join("scratch")));

        let allocated = allocate_scratch_dir(&home, None).unwrap();
        fs::write(home.join("scratch/ignore-file.txt"), "note\n").unwrap();
        let listed = list_scratch_dirs(&home).unwrap();
        assert_eq!(listed, vec![allocated.clone()]);
        assert!(is_scratch_dir(&home, &allocated));
        assert!(is_scratch_dir(&home, &allocated.canonicalize().unwrap()));
        assert!(!is_scratch_dir(&home, &home.join("worktrees/repo/wt")));
    }
}
