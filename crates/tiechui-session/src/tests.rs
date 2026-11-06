use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

use serde_json::json;
use tempfile::TempDir;
use tiechui_model::{Content, Message, ModelId, OpaqueData, ReasoningLevel, Role, Usage};

use super::*;

struct Fixture {
    _root: TempDir,
    home: PathBuf,
    cwd: PathBuf,
    cancel: CancellationToken,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let cwd = root.path().join("project");
        fs::create_dir(&cwd).unwrap();
        Self {
            home: root.path().join("home"),
            cwd,
            _root: root,
            cancel: CancellationToken::new(),
        }
    }

    fn create(&self) -> SessionStore {
        SessionStore::create(&self.home, &self.cwd, &self.cancel).unwrap()
    }
}

fn model() -> ModelRef {
    ModelRef {
        provider: "test-provider".into(),
        model: ModelId("test/model".into()),
    }
}

#[test]
fn reload_retains_writer_lock_and_context_exclusions_preserve_history() {
    let fixture = Fixture::new();
    let mut store = fixture.create();
    let first = add(&mut store, "retained", &fixture.cancel);
    let omitted = add(&mut store, "audit only", &fixture.cancel);
    let excluded = HashSet::from([omitted.clone()]);
    assert_eq!(
        store.build_context_excluding(&excluded),
        vec![message(Role::User, "retained")]
    );
    assert_eq!(store.entries().len(), 2);
    store.branch_to(&first).unwrap();
    store.reload(&fixture.cancel).unwrap();
    assert_eq!(store.current_leaf(), Some(omitted.as_str()));
    assert!(SessionStore::open(store.path(), &fixture.cancel).is_err());
    add(&mut store, "after reload", &fixture.cancel);
    assert_eq!(store.build_context().len(), 3);
    fixture.cancel.cancel();
    assert!(store.reload(&fixture.cancel).is_err());
    assert!(SessionStore::open(store.path(), &CancellationToken::new()).is_err());
}

fn message(role: Role, text: &str) -> Message {
    Message {
        role,
        content: vec![Content::Text {
            text: text.into(),
            opaque: None,
        }],
        opaque: None,
    }
}

fn add(store: &mut SessionStore, text: &str, cancel: &CancellationToken) -> String {
    store
        .append(
            EntryKind::Message {
                reasoning: None,
                usage: None,
                stop_reason: None,
                details: None,
                message: message(Role::User, text),
                model: None,
            },
            cancel,
        )
        .unwrap()
        .id
        .clone()
}

fn compact(
    store: &mut SessionStore,
    summary: &str,
    first: Option<String>,
    cancel: &CancellationToken,
) -> String {
    store
        .append(
            EntryKind::Compaction {
                tokens_before: 0,
                usage: None,
                details: serde_json::Value::Null,
                from_extension: false,
                summary: summary.into(),
                first_kept_entry_id: first,
                system_message: None,
            },
            cancel,
        )
        .unwrap()
        .id
        .clone()
}

fn text(message: &Message) -> &str {
    let Content::Text { text, .. } = &message.content[0] else {
        panic!("expected text")
    };
    text
}

fn raw_append(path: &Path, bytes: &[u8]) {
    OpenOptions::new()
        .append(true)
        .open(path)
        .unwrap()
        .write_all(bytes)
        .unwrap();
}

#[test]
fn reload_matches_every_entry_kind_and_context() {
    let fixture = Fixture::new();
    let mut store = fixture.create();
    let root = add(&mut store, "question", &fixture.cancel);
    for kind in [
        EntryKind::ModelChange { model: model() },
        EntryKind::ThinkingChange {
            level: Some(ReasoningLevel::High),
        },
        EntryKind::ThinkingChange { level: None },
        EntryKind::Message {
            reasoning: Some(ReasoningLevel::High),
            usage: None,
            stop_reason: None,
            details: None,
            message: message(Role::Assistant, "answer"),
            model: Some(model()),
        },
        EntryKind::Usage {
            request_id: None,
            model: None,
            usage: Usage {
                input: 40,
                output: 10,
                cache_read: 5,
                cache_write: 2,
                ..Default::default()
            },
        },
        EntryKind::Label {
            target_id: root.clone(),
            label: Some("bookmark".into()),
        },
        EntryKind::Label {
            target_id: root.clone(),
            label: None,
        },
        EntryKind::Custom {
            kind: "module-state".into(),
            data: json!({"value": [false, 4, null]}),
        },
        EntryKind::Compaction {
            tokens_before: 0,
            usage: None,
            details: serde_json::Value::Null,
            from_extension: false,
            summary: "history".into(),
            first_kept_entry_id: Some(root),
            system_message: None,
        },
    ] {
        store.append(kind, &fixture.cancel).unwrap();
    }
    let entries = store.entries().to_vec();
    let header = store.header().clone();
    let leaf = store.current_leaf().unwrap().to_owned();
    let context = store.build_context();
    let path = store.path().to_owned();
    drop(store);
    let reloaded = SessionStore::open(&path, &fixture.cancel).unwrap();
    assert_eq!(reloaded.header(), &header);
    assert_eq!(reloaded.entries(), entries);
    assert_eq!(reloaded.current_leaf(), Some(leaf.as_str()));
    assert_eq!(reloaded.build_context(), context);
    assert!(reloaded.diagnostics().is_empty());
}

#[test]
fn assistant_thinking_and_opaque_replay_survive_reload_context_and_fork() {
    let fixture = Fixture::new();
    let mut store = fixture.create();
    let opaque = OpaqueData("  {\"signature\":\"a\\n+/=\",\"unknown\": [1,  2]}\r\n\0汉字".into());
    let thinking = Message {
        role: Role::Assistant,
        content: vec![
            Content::Thinking {
                text: "think\r\n\0e\u{301}".into(),
                redacted: false,
                opaque: Some(opaque.clone()),
            },
            Content::Thinking {
                text: String::new(),
                redacted: true,
                opaque: Some(opaque.clone()),
            },
            Content::Text {
                text: "answer".into(),
                opaque: Some(opaque.clone()),
            },
        ],
        opaque: Some(opaque),
    };
    let kind = EntryKind::Message {
        reasoning: None,
        usage: None,
        stop_reason: None,
        details: None,
        message: thinking.clone(),
        model: Some(model()),
    };
    let id = store
        .append(kind.clone(), &fixture.cancel)
        .unwrap()
        .id
        .clone();
    let path = store.path().to_owned();
    drop(store);
    let store = SessionStore::open(&path, &fixture.cancel).unwrap();
    let fork = store.fork(&id, &fixture.cancel).unwrap();
    for candidate in [&store, &fork] {
        assert_eq!(candidate.entries()[0].kind, kind);
        assert_eq!(candidate.build_context(), vec![thinking.clone()]);
        let EntryKind::Message {
            message,
            model: origin,
            ..
        } = &candidate.entries()[0].kind
        else {
            panic!()
        };
        assert_eq!(origin, &Some(model()));
        for (actual, expected) in message.content.iter().zip(&thinking.content) {
            if let (
                Content::Thinking {
                    text: a,
                    redacted: ar,
                    opaque: ao,
                },
                Content::Thinking {
                    text: b,
                    redacted: br,
                    opaque: bo,
                },
            ) = (actual, expected)
            {
                assert_eq!(a.as_bytes(), b.as_bytes());
                assert_eq!(ar, br);
                assert_eq!(
                    ao.as_ref().unwrap().0.as_bytes(),
                    bo.as_ref().unwrap().0.as_bytes()
                );
            }
        }
    }
}

#[test]
fn assistant_requires_origin_on_append_and_load() {
    let fixture = Fixture::new();
    let mut store = fixture.create();
    let kind = EntryKind::Message {
        reasoning: None,
        usage: None,
        stop_reason: None,
        details: None,
        message: message(Role::Assistant, "answer"),
        model: None,
    };
    assert!(
        store
            .append(kind.clone(), &fixture.cancel)
            .unwrap_err()
            .message
            .contains("originating model")
    );
    assert!(store.entries().is_empty());
    let entry = Entry {
        id: "assistant".into(),
        parent_id: None,
        timestamp: 1,
        kind,
    };
    let mut line = serde_json::to_vec(&entry).unwrap();
    line.push(b'\n');
    let path = store.path().to_owned();
    drop(store);
    raw_append(&path, &line);
    let error = SessionStore::open(&path, &fixture.cancel).unwrap_err();
    assert_eq!(error.line, Some(2));
    assert!(error.message.contains("originating model"));
}

#[test]
fn branch_then_append_retains_both_children_and_reload_selects_new_leaf() {
    let fixture = Fixture::new();
    let mut store = fixture.create();
    let a = add(&mut store, "a", &fixture.cancel);
    let b = add(&mut store, "b", &fixture.cancel);
    store.branch_to(&a).unwrap();
    let c = add(&mut store, "c", &fixture.cancel);
    assert_eq!(store.entries()[1].parent_id.as_deref(), Some(a.as_str()));
    assert_eq!(store.entries()[2].parent_id.as_deref(), Some(a.as_str()));
    assert_eq!(
        store
            .path_to_root(&b)
            .unwrap()
            .iter()
            .map(|e| e.id.as_str())
            .collect::<Vec<_>>(),
        [a.as_str(), b.as_str()]
    );
    assert_eq!(
        store.build_context().iter().map(text).collect::<Vec<_>>(),
        ["a", "c"]
    );
    assert!(store.branch_to("missing").is_err());
    assert_eq!(store.current_leaf(), Some(c.as_str()));
    let path = store.path().to_owned();
    drop(store);
    let store = SessionStore::open(&path, &fixture.cancel).unwrap();
    assert_eq!(store.current_leaf(), Some(c.as_str()));
    assert_eq!(store.entries().len(), 3);
}

#[test]
fn fork_copies_exact_selected_path_including_labels_and_metadata() {
    let fixture = Fixture::new();
    let mut store = fixture.create();
    let a = add(&mut store, "a", &fixture.cancel);
    let label = store
        .append(
            EntryKind::Label {
                target_id: a.clone(),
                label: Some("a".into()),
            },
            &fixture.cancel,
        )
        .unwrap()
        .id
        .clone();
    let b = add(&mut store, "b", &fixture.cancel);
    store.branch_to(&a).unwrap();
    add(&mut store, "other branch", &fixture.cancel);
    let fork = store.fork(&b, &fixture.cancel).unwrap();
    assert_eq!(
        fork.entries().iter().collect::<Vec<_>>(),
        store.path_to_root(&b).unwrap()
    );
    assert_eq!(fork.entries()[2].parent_id.as_deref(), Some(label.as_str()));
    assert_eq!(fork.current_leaf(), Some(b.as_str()));
    assert_ne!(fork.header().id, store.header().id);
    assert_eq!(fork.header().parent_session.as_deref(), Some(store.path()));
    assert_eq!(fork.header().cwd, store.header().cwd);
    let path = fork.path().to_owned();
    let expected = fork.entries().to_vec();
    drop(fork);
    assert_eq!(
        SessionStore::open(&path, &fixture.cancel)
            .unwrap()
            .entries(),
        expected
    );
    assert!(store.fork("missing", &fixture.cancel).is_err());
    let empty = store.fork(&store.header().id, &fixture.cancel).unwrap();
    assert!(empty.entries().is_empty());
}

#[test]
fn random_branch_sequences_always_reach_implicit_header_without_cycles() {
    let fixture = Fixture::new();
    // Fixed seeds make failures reproducible without a random-number dependency.
    for seed in 0..8_u64 {
        let mut state = seed;
        let mut store = fixture.create();
        let mut ids = vec![store.header().id.clone()];
        for _ in 0..100 {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            store.branch_to(&ids[(state as usize) % ids.len()]).unwrap();
            ids.push(add(&mut store, "entry", &fixture.cancel));
            for id in &ids {
                let path = store.path_to_root(id).unwrap();
                if path.is_empty() {
                    assert_eq!(id, &store.header().id);
                    continue;
                }
                assert_eq!(&path.last().unwrap().id, id);
                assert!(path[0].parent_id.is_none());
                assert!(path.len() < ids.len());
                for pair in path.windows(2) {
                    assert_eq!(pair[1].parent_id.as_deref(), Some(pair[0].id.as_str()));
                }
            }
        }
        let path = store.path().to_owned();
        let entries = store.entries().to_vec();
        drop(store);
        assert_eq!(
            SessionStore::open(&path, &fixture.cancel)
                .unwrap()
                .entries(),
            entries
        );
    }
}

#[test]
fn torn_tail_is_reported_ignored_and_only_repaired_on_append() {
    let fixture = Fixture::new();
    let mut store = fixture.create();
    add(&mut store, "before", &fixture.cancel);
    let path = store.path().to_owned();
    drop(store);
    raw_append(&path, b"{\"id\":\"torn\",\"kind\":\"mess");
    let original = fs::read(&path).unwrap();
    let mut reader = SessionStore::open_read_only(&path, &fixture.cancel).unwrap();
    assert_eq!(reader.entries().len(), 1);
    assert_eq!(reader.diagnostics()[0].line, 3);
    assert!(
        reader
            .append(
                EntryKind::Usage {
                    request_id: None,
                    model: None,
                    usage: Usage::default()
                },
                &fixture.cancel
            )
            .is_err()
    );
    assert_eq!(fs::read(&path).unwrap(), original);
    let mut writer = SessionStore::open(&path, &fixture.cancel).unwrap();
    assert_eq!(fs::read(&path).unwrap(), original);
    add(&mut writer, "after", &fixture.cancel);
    drop(writer);
    let store = SessionStore::open(&path, &fixture.cancel).unwrap();
    assert!(store.diagnostics().is_empty());
    assert_eq!(
        store.build_context().iter().map(text).collect::<Vec<_>>(),
        ["before", "after"]
    );
    assert_eq!(fs::read(&path).unwrap().split(|&b| b == b'\n').count(), 4);
}

#[test]
fn complete_last_json_without_newline_is_preserved_before_append() {
    let fixture = Fixture::new();
    let mut store = fixture.create();
    add(&mut store, "before", &fixture.cancel);
    let path = store.path().to_owned();
    drop(store);
    let mut bytes = fs::read(&path).unwrap();
    assert_eq!(bytes.pop(), Some(b'\n'));
    fs::write(&path, &bytes).unwrap();
    let mut store = SessionStore::open(&path, &fixture.cancel).unwrap();
    assert!(store.diagnostics().is_empty());
    add(&mut store, "after", &fixture.cancel);
    assert!(fs::read(&path).unwrap().starts_with(&bytes));
    drop(store);
    assert_eq!(
        SessionStore::open(&path, &fixture.cancel)
            .unwrap()
            .entries()
            .len(),
        2
    );
}

#[test]
fn corruption_reports_line_and_never_discards_complete_or_middle_lines() {
    let fixture = Fixture::new();
    for tail in [
        b"broken\n".as_slice(),
        b"broken\n{}\n",
        b"{}",
        b"\n",
        b"{\"id\":\"x\",\"timestamp\":0,\"parent_id\":null,\"kind\":\"message\"}\n",
    ] {
        let store = fixture.create();
        let path = store.path().to_owned();
        drop(store);
        raw_append(&path, tail);
        let original = fs::read(&path).unwrap();
        let error = SessionStore::open(&path, &fixture.cancel).unwrap_err();
        assert_eq!(error.line, Some(2));
        assert!(error.to_string().contains(":2:"));
        assert_eq!(fs::read(&path).unwrap(), original);
        // A failed load releases the lock.
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        file.try_lock().unwrap();
    }
}

#[test]
fn bad_header_duplicate_or_orphan_entry_is_a_line_numbered_error() {
    let fixture = Fixture::new();
    for invalid in [
        json!({"id":"x","parent_id":"missing","timestamp":1,"kind":"usage","usage":Usage::default()}),
        json!({"id":"","parent_id":null,"timestamp":1,"kind":"usage","usage":Usage::default()}),
    ] {
        let store = fixture.create();
        let path = store.path().to_owned();
        drop(store);
        raw_append(&path, format!("{invalid}\n").as_bytes());
        assert_eq!(
            SessionStore::open(&path, &fixture.cancel).unwrap_err().line,
            Some(2)
        );
    }
    let mut store = fixture.create();
    add(&mut store, "entry", &fixture.cancel);
    let line = store.lines[0].clone();
    let path = store.path().to_owned();
    drop(store);
    raw_append(&path, &line);
    assert_eq!(
        SessionStore::open(&path, &fixture.cancel).unwrap_err().line,
        Some(3)
    );
    fs::write(
        &path,
        b"{\"format_version\":99,\"id\":\"header\",\"cwd\":\"/test\",\"created_at\":0}\n",
    )
    .unwrap();
    assert_eq!(
        SessionStore::open(&path, &fixture.cancel).unwrap_err().line,
        Some(1)
    );
    fs::write(&path, b"{\"format_version\":").unwrap();
    assert_eq!(
        SessionStore::open(&path, &fixture.cancel).unwrap_err().line,
        Some(1)
    );
}

#[test]
fn unknown_entry_is_ignored_in_context_and_copied_byte_for_byte() {
    let fixture = Fixture::new();
    let store = fixture.create();
    let path = store.path().to_owned();
    drop(store);
    let unknown = b"{ \"id\" : \"future\", \"parent_id\":null, \"timestamp\":7, \"kind\":\"future_kind\", \"payload\": {\"float\":1.00e+02,\"escaped\":\"\\u0061\", \"nested\":[1, null, true]} }\r\n";
    raw_append(&path, unknown);
    let mut store = SessionStore::open(&path, &fixture.cancel).unwrap();
    let EntryKind::Unknown { kind, fields } = &store.entries()[0].kind else {
        panic!()
    };
    assert_eq!(kind, "future_kind");
    assert_eq!(fields["payload"]["escaped"], "a");
    assert!(store.build_context().is_empty());
    let id = add(&mut store, "after", &fixture.cancel);
    let fork = store.fork(&id, &fixture.cancel).unwrap();
    let bytes = fs::read(fork.path()).unwrap();
    let first_end = bytes.iter().position(|&b| b == b'\n').unwrap() + 1;
    assert_eq!(&bytes[first_end..first_end + unknown.len()], unknown);
    assert_eq!(fork.entries(), store.entries());
    let source_bytes = fs::read(store.path()).unwrap();
    let source_header_end = source_bytes.iter().position(|&b| b == b'\n').unwrap() + 1;
    assert_eq!(
        &source_bytes[source_header_end..source_header_end + unknown.len()],
        unknown
    );
}

#[test]
fn writer_lock_lasts_until_drop_and_read_only_open_can_coexist() {
    let fixture = Fixture::new();
    let store = fixture.create();
    let path = store.path().to_owned();
    assert!(
        SessionStore::open(&path, &fixture.cancel)
            .unwrap_err()
            .message
            .contains("writer lock")
    );
    let reader = SessionStore::open_read_only(&path, &fixture.cancel).unwrap();
    assert!(reader.entries().is_empty());
    assert!(SessionStore::open(&path, &fixture.cancel).is_err());
    drop(store);
    let _writer = SessionStore::open(&path, &fixture.cancel).unwrap();
}

#[cfg(unix)]
#[test]
fn symlink_cannot_bypass_writer_lock() {
    let fixture = Fixture::new();
    let store = fixture.create();
    let alias = fixture.home.join("alias.jsonl");
    std::os::unix::fs::symlink(store.path(), &alias).unwrap();
    assert!(
        SessionStore::open(&alias, &fixture.cancel)
            .unwrap_err()
            .message
            .contains("writer lock")
    );
}

#[cfg(unix)]
#[test]
fn created_files_and_all_new_directories_are_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new();
    let store = fixture.create();
    assert_eq!(
        fs::metadata(store.path()).unwrap().permissions().mode() & 0o777,
        0o600
    );
    for directory in [
        fixture.home.clone(),
        fixture.home.join("sessions"),
        store.path().parent().unwrap().to_owned(),
    ] {
        assert_eq!(
            fs::metadata(directory).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
}

#[test]
fn list_reads_only_headers_even_with_corrupt_bodies_and_active_writers() {
    let fixture = Fixture::new();
    assert!(
        list(&fixture.home, &fixture.cwd, &fixture.cancel)
            .unwrap()
            .sessions
            .is_empty()
    );
    assert!(!fixture.home.exists());
    let store = fixture.create();
    let other = fixture.create();
    raw_append(store.path(), b"invalid body\n");
    let listed = list(&fixture.home, &fixture.cwd, &fixture.cancel).unwrap();
    assert_eq!(listed.sessions.len(), 2);
    assert!(
        listed
            .sessions
            .iter()
            .any(|info| &info.header == store.header())
    );
    assert!(
        listed
            .sessions
            .iter()
            .any(|info| &info.header == other.header())
    );
    assert!(listed.diagnostics.is_empty());
    assert!(SessionStore::open_read_only(store.path(), &fixture.cancel).is_err());
}

#[test]
fn regression_list_keeps_valid_sessions_and_reports_bad_headers() {
    let fixture = Fixture::new();
    let store = fixture.create();
    let directory = store.path().parent().unwrap();
    for (name, content) in [
        ("corrupt.jsonl", b"invalid header\n".as_slice()),
        ("creating.jsonl", b"".as_slice()),
        ("partial.jsonl", b"{\"format_version\":".as_slice()),
    ] {
        fs::write(directory.join(name), content).unwrap();
    }
    let listed = list(&fixture.home, &fixture.cwd, &fixture.cancel).unwrap();
    assert_eq!(listed.sessions.len(), 1);
    assert_eq!(listed.sessions[0].header, *store.header());
    assert_eq!(listed.diagnostics.len(), 3);
    assert_eq!(
        listed
            .diagnostics
            .iter()
            .map(|diagnostic| diagnostic.path.file_name().unwrap().to_str().unwrap())
            .collect::<Vec<_>>(),
        ["corrupt.jsonl", "creating.jsonl", "partial.jsonl"]
    );
    for diagnostic in &listed.diagnostics {
        assert_eq!(diagnostic.line, Some(1));
        assert!(!diagnostic.message.is_empty());
    }
    // Discovery must not repair a source that may still be being created.
    assert_eq!(fs::read(directory.join("creating.jsonl")).unwrap(), b"");
    assert_eq!(
        fs::read(directory.join("partial.jsonl")).unwrap(),
        b"{\"format_version\":"
    );
}

#[test]
fn regression_long_chinese_cwd_can_create_and_list_sessions() {
    let fixture = Fixture::new();
    let cwd = fixture.cwd.join("中文目录".repeat(12)).join("项目会话");
    fs::create_dir_all(&cwd).unwrap();
    let store = SessionStore::create(&fixture.home, &cwd, &fixture.cancel).unwrap();
    let directory = session_dir(&fixture.home, &cwd).unwrap();
    assert_eq!(store.path().parent(), Some(directory.as_path()));
    assert!(directory.file_name().unwrap().len() <= 137);
    assert_eq!(store.header().cwd, cwd.canonicalize().unwrap());
    let listed = list(&fixture.home, &cwd, &fixture.cancel).unwrap();
    assert_eq!(listed.sessions.len(), 1);
    assert!(listed.diagnostics.is_empty());
    let other_cwd = cwd.with_file_name("另一个项目");
    fs::create_dir(&other_cwd).unwrap();
    assert_ne!(directory, session_dir(&fixture.home, &other_cwd).unwrap());
    assert_eq!(
        directory,
        session_dir(&fixture.home, &cwd.join(".")).unwrap()
    );
}

#[test]
fn cwd_encoding_is_stable_and_distinguishes_similar_paths() {
    let fixture = Fixture::new();
    let left = fixture.cwd.join("a-b");
    let right = fixture.cwd.join("a/b");
    fs::create_dir_all(&left).unwrap();
    fs::create_dir_all(&right).unwrap();
    let encoded_left = session_dir(&fixture.home, &left).unwrap();
    let encoded_right = session_dir(&fixture.home, &right).unwrap();
    assert_ne!(encoded_left, encoded_right);
    assert_eq!(
        encoded_left,
        session_dir(&fixture.home, &left.join(".")).unwrap()
    );
    assert!(encoded_left.starts_with(fixture.home.join("sessions")));
    assert!(
        encoded_right
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .contains("%2F")
    );
}

#[test]
fn cancelled_operations_do_not_create_or_modify_sessions() {
    let fixture = Fixture::new();
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    assert!(SessionStore::create(&fixture.home, &fixture.cwd, &cancelled).is_err());
    assert!(!fixture.home.exists());
    let mut store = fixture.create();
    let id = add(&mut store, "before", &fixture.cancel);
    let original = fs::read(store.path()).unwrap();
    assert!(
        store
            .append(
                EntryKind::Usage {
                    request_id: None,
                    model: None,
                    usage: Usage::default()
                },
                &cancelled
            )
            .is_err()
    );
    assert!(store.fork(&id, &cancelled).is_err());
    assert!(SessionStore::open(store.path(), &cancelled).is_err());
    assert!(SessionStore::open_read_only(store.path(), &cancelled).is_err());
    assert!(list(&fixture.home, &fixture.cwd, &cancelled).is_err());
    assert_eq!(fs::read(store.path()).unwrap(), original);
    assert_eq!(
        list(&fixture.home, &fixture.cwd, &fixture.cancel)
            .unwrap()
            .sessions
            .len(),
        1
    );
}

#[test]
fn unexpected_external_append_is_rejected_without_changing_memory() {
    let fixture = Fixture::new();
    let mut store = fixture.create();
    raw_append(store.path(), b"external\n");
    let original = fs::read(store.path()).unwrap();
    assert!(
        store
            .append(
                EntryKind::Usage {
                    request_id: None,
                    model: None,
                    usage: Usage::default()
                },
                &fixture.cancel
            )
            .unwrap_err()
            .message
            .contains("changed outside")
    );
    assert!(store.entries().is_empty());
    assert_eq!(fs::read(store.path()).unwrap(), original);
}

#[test]
fn compaction_retains_from_anchor_filters_old_system_and_keeps_later_system() {
    let fixture = Fixture::new();
    let mut store = fixture.create();
    add(&mut store, "discard", &fixture.cancel);
    let kept = add(&mut store, "keep", &fixture.cancel);
    store
        .append(
            EntryKind::Message {
                reasoning: None,
                usage: None,
                stop_reason: None,
                details: None,
                message: message(Role::System, "old system"),
                model: None,
            },
            &fixture.cancel,
        )
        .unwrap();
    add(&mut store, "also keep", &fixture.cancel);
    compact(&mut store, "summary", Some(kept), &fixture.cancel);
    store
        .append(
            EntryKind::Message {
                reasoning: None,
                usage: None,
                stop_reason: None,
                details: None,
                message: message(Role::System, "new system"),
                model: None,
            },
            &fixture.cancel,
        )
        .unwrap();
    let context = store.build_context();
    assert_eq!(context.len(), 5);
    assert_eq!(context[0], message(Role::System, "old system"));
    assert_eq!(context[1].role, Role::User);
    assert_eq!(
        text(&context[1]),
        "The conversation history before this point was compacted into the following summary:\n\n<summary>\nsummary\n</summary>"
    );
    assert_eq!(
        context[2..].iter().map(text).collect::<Vec<_>>(),
        ["keep", "also keep", "new system"]
    );
    assert_eq!(context[4].role, Role::System);
    // The next compaction checkpoints the later system message, even when its
    // retained range includes the previous compaction and its older checkpoint.
    let first = store.entries()[0].id.clone();
    let newest = compact(&mut store, "newest", Some(first), &fixture.cancel);
    let context = store.build_context();
    assert_eq!(context[0], message(Role::System, "new system"));
    assert!(text(&context[1]).contains("\nnewest\n"));
    assert_eq!(
        context[2..].iter().map(text).collect::<Vec<_>>(),
        ["discard", "keep", "also keep"]
    );
    let fork = store.fork(&newest, &fixture.cancel).unwrap();
    assert_eq!(fork.build_context(), context);
    let path = store.path().to_owned();
    drop(store);
    assert_eq!(
        SessionStore::open(&path, &fixture.cancel)
            .unwrap()
            .build_context(),
        context
    );
}

#[test]
fn compaction_anchor_table_includes_non_message_and_self_ids() {
    let fixture = Fixture::new();
    for anchor in [
        "first", "metadata", "last", "none", "self", "missing", "later",
    ] {
        let mut store = fixture.create();
        let first = add(&mut store, "first", &fixture.cancel);
        let metadata = store
            .append(
                EntryKind::Custom {
                    kind: "module".into(),
                    data: json!(null),
                },
                &fixture.cancel,
            )
            .unwrap()
            .id
            .clone();
        let last = add(&mut store, "last", &fixture.cancel);
        let first_kept = match anchor {
            "first" => Some(first),
            "metadata" => Some(metadata),
            "last" => Some(last),
            "none" => None,
            "self" => Some("self-compaction".into()),
            "later" => Some("later-entry".into()),
            _ => Some("absent".into()),
        };
        // Self-referential compactions are a sentinel, so exercise the persisted form.
        let entry = Entry {
            id: "self-compaction".into(),
            parent_id: store.leaf.clone(),
            timestamp: 1,
            kind: EntryKind::Compaction {
                tokens_before: 0,
                usage: None,
                details: serde_json::Value::Null,
                from_extension: false,
                summary: "summary".into(),
                first_kept_entry_id: first_kept,
                system_message: None,
            },
        };
        let mut line = serde_json::to_vec(&entry).unwrap();
        line.push(b'\n');
        store.write_entry(entry, &line, &fixture.cancel).unwrap();
        let later = Entry {
            id: "later-entry".into(),
            parent_id: store.leaf.clone(),
            timestamp: 2,
            kind: EntryKind::Message {
                reasoning: None,
                usage: None,
                stop_reason: None,
                details: None,
                message: message(Role::User, "after"),
                model: None,
            },
        };
        let mut line = serde_json::to_vec(&later).unwrap();
        line.push(b'\n');
        store.write_entry(later, &line, &fixture.cancel).unwrap();
        let context = store.build_context();
        let expected = match anchor {
            "first" => vec!["first", "last", "after"],
            "metadata" | "last" => vec!["last", "after"],
            _ => vec!["after"],
        };
        assert_eq!(
            context[1..].iter().map(text).collect::<Vec<_>>(),
            expected,
            "anchor={anchor}"
        );
    }
}

#[test]
fn latest_compaction_only_and_active_branch_determine_context() {
    let fixture = Fixture::new();
    let mut store = fixture.create();
    let first = add(&mut store, "first", &fixture.cancel);
    let earlier = compact(&mut store, "old", Some(first.clone()), &fixture.cancel);
    add(&mut store, "second", &fixture.cancel);
    compact(&mut store, "new", Some(first.clone()), &fixture.cancel);
    add(&mut store, "third", &fixture.cancel);
    let context = store.build_context();
    assert!(text(&context[0]).contains("\nnew\n"));
    assert_eq!(
        context[1..].iter().map(text).collect::<Vec<_>>(),
        ["first", "second", "third"]
    );
    store.branch_to(&earlier).unwrap();
    let context = store.build_context();
    assert!(text(&context[0]).contains("\nold\n"));
    assert_eq!(text(&context[1]), "first");
    store.branch_to(&first).unwrap();
    assert_eq!(store.build_context(), vec![message(Role::User, "first")]);
    let header_id = store.header().id.clone();
    store.branch_to(&header_id).unwrap();
    assert!(store.build_context().is_empty());
    assert!(store.current_leaf().is_none());
}

#[test]
fn browsing_reads_the_current_branch_without_touching_the_file() {
    let fixture = Fixture::new();
    let _empty = fixture.create();
    let mut store = fixture.create();
    add(&mut store, "first question", &fixture.cancel);
    let answer = Message {
        role: Role::Assistant,
        content: vec![
            Content::Thinking {
                text: "private".into(),
                redacted: false,
                opaque: None,
            },
            Content::Text {
                text: "Looking.".into(),
                opaque: None,
            },
            Content::ToolCall(tiechui_model::ToolCall {
                id: "call-1".into(),
                name: "read".into(),
                arguments: json!({"path": "a.txt"}),
                opaque: None,
            }),
        ],
        opaque: None,
    };
    store
        .append(
            EntryKind::Message {
                reasoning: None,
                usage: None,
                stop_reason: None,
                details: None,
                message: answer,
                model: Some(model()),
            },
            &fixture.cancel,
        )
        .unwrap();
    let result = Message {
        role: Role::Tool,
        content: vec![Content::ToolResult(tiechui_model::ToolResult {
            call_id: "call-1".into(),
            content: vec![
                Content::Text {
                    text: "contents".into(),
                    opaque: None,
                },
                Content::Image {
                    mime_type: "image/png".into(),
                    data: "AAAA".into(),
                },
            ],
            is_error: false,
        })],
        opaque: None,
    };
    store
        .append(
            EntryKind::Message {
                reasoning: None,
                usage: None,
                stop_reason: None,
                details: None,
                message: result,
                model: None,
            },
            &fixture.cancel,
        )
        .unwrap();
    let fork = compact(&mut store, "earlier work", None, &fixture.cancel);
    add(&mut store, "abandoned", &fixture.cancel);
    store.branch_to(&fork).unwrap();
    add(&mut store, "kept", &fixture.cancel);
    let path = store.path().to_owned();
    let bytes = fs::read(&path).unwrap();
    let modified = fs::metadata(&path).unwrap().modified().unwrap();

    let part = |kind, text: &str| Part {
        kind,
        text: text.into(),
    };
    assert_eq!(
        preview(&path, &fixture.cancel).unwrap(),
        vec![
            part(PartKind::User, "first question"),
            part(
                PartKind::Assistant,
                "Looking.\n\nCalled read\n{\n  \"path\": \"a.txt\"\n}"
            ),
            part(
                PartKind::Tool,
                "contents\n\n[image: image/png; data omitted]"
            ),
            part(PartKind::Notice, "earlier work"),
            part(PartKind::User, "kept"),
        ]
    );
    let (listed, diagnostics) =
        summaries(&fixture.home, Some(&fixture.cwd), &fixture.cancel).unwrap();
    assert!(diagnostics.is_empty());
    assert_eq!(listed.len(), 1, "the session without messages is left out");
    let summary = &listed[0];
    assert_eq!(summary.path, path);
    assert_eq!(summary.first_message, "first question");
    assert_eq!(summary.text, "first question Looking. abandoned kept");
    assert_eq!(summary.modified, store.entries().last().unwrap().timestamp);
    let (everywhere, _) = summaries(&fixture.home, None, &fixture.cancel).unwrap();
    assert_eq!(everywhere, listed);

    assert_eq!(fs::read(&path).unwrap(), bytes);
    assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), modified);
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    assert!(preview(&path, &cancelled).is_err());
    assert!(summaries(&fixture.home, None, &cancelled).is_err());
}

#[cfg(unix)]
#[test]
fn lexical_cwd_survives_resume_and_both_spellings_match_the_filter() {
    let fixture = Fixture::new();
    let alias = fixture._root.path().join("alias");
    std::os::unix::fs::symlink(&fixture.cwd, &alias).unwrap();
    let mut lexical =
        SessionStore::create(&fixture.home, &alias.join("."), &fixture.cancel).unwrap();
    add(&mut lexical, "lexical", &fixture.cancel);
    assert_eq!(lexical.header().cwd, alias);
    let lexical_id = lexical.header().id.clone();
    let lexical_path = lexical.path().to_owned();
    drop(lexical);
    let reopened = SessionStore::open(&lexical_path, &fixture.cancel).unwrap();
    assert_eq!(reopened.header().cwd, alias);
    drop(reopened);
    let mut canonical = fixture.create();
    add(&mut canonical, "canonical", &fixture.cancel);
    assert_eq!(canonical.header().cwd, fixture.cwd);
    assert_eq!(canonical.path().parent(), lexical_path.parent());
    for cwd in [&alias, &fixture.cwd] {
        let listed = list(&fixture.home, cwd, &fixture.cancel).unwrap();
        assert_eq!(listed.sessions.len(), 2);
        assert!(
            listed
                .sessions
                .iter()
                .any(|session| session.header.id == lexical_id)
        );
        let (summaries, errors) = summaries(&fixture.home, Some(cwd), &fixture.cancel).unwrap();
        assert!(errors.is_empty());
        assert_eq!(summaries.len(), 2);
    }
}
