#![allow(clippy::unwrap_used)]

use std::fs;
use std::path::Path;
use std::sync::Arc;

use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use tiechui_model::{Content, ToolCall};
use tiechui_module_editing::{register, register_with_image_resize};
use tiechui_protocol::{SessionEpoch, ThreadId, TurnId};
use tiechui_tools::{
    Concurrency, ExecutionScope, ToolContext, ToolError, ToolOutput, ToolRegistry,
};
use tokio_util::sync::CancellationToken;

fn context(cwd: &Path) -> ToolContext {
    ToolContext {
        invoker: None,
        branch: Default::default(),
        parent_session: None,
        cwd: cwd.into(),
        cancel: CancellationToken::new(),
        updates: Arc::new(|_| {}),
        append_entry: Arc::new(|_| Ok(())),
        epoch: SessionEpoch(0),
        scope: ExecutionScope {
            thread_id: ThreadId("editing-test".into()),
            turn_id: TurnId("turn".into()),
        },
        origin: Default::default(),
    }
}
async fn run(
    registry: &ToolRegistry,
    ctx: &ToolContext,
    name: &str,
    input: Value,
) -> Result<ToolOutput, ToolError> {
    registry
        .get(name)
        .unwrap()
        .run(
            ToolCall {
                id: "read-id".into(),
                name: name.into(),
                arguments: input,
                opaque: None,
            },
            ctx.clone(),
        )
        .await
}
fn fixture_output(output: ToolOutput) -> Value {
    let content: Vec<_> = output
        .content
        .into_iter()
        .map(|c| match c {
            Content::Text { text, .. } => json!({"type":"text","text":text}),
            Content::Image { mime_type, data } => {
                json!({"type":"image","mimeType":mime_type,"data":data})
            }
            _ => panic!("Unexpected content"),
        })
        .collect();
    let mut value = json!({"content":content});
    if let Some(details) = output.details {
        value["details"] = details;
    }
    if let Some(content) = output.structured_content {
        value["structuredContent"] = content;
    }
    value
}
fn text(output: &ToolOutput) -> &str {
    match &output.content[0] {
        Content::Text { text, .. } => text,
        _ => panic!(),
    }
}

#[tokio::test]
async fn real_typescript_goldens_match_content_details_and_written_bytes() {
    let fixture: Value = serde_json::from_str(include_str!("fixtures/golden.json")).unwrap();
    let directory = tempfile::tempdir().unwrap();
    let cwd = directory.path();
    fs::create_dir(cwd.join(".git")).unwrap();
    for (name, value) in fixture["files"].as_object().unwrap() {
        let bytes = if let Some(text) = value.as_str() {
            text.as_bytes().to_vec()
        } else if let Some(array) = value.as_array() {
            array.iter().map(|v| v.as_u64().unwrap() as u8).collect()
        } else {
            STANDARD.decode(value["base64"].as_str().unwrap()).unwrap()
        };
        fs::write(cwd.join(name), bytes).unwrap();
    }
    let mut registry = ToolRegistry::default();
    let state = register_with_image_resize(&mut registry, false).unwrap();
    let mut resized_registry = ToolRegistry::default();
    register(&mut resized_registry).unwrap();
    let ctx = context(cwd);
    for (i, case) in fixture["cases"].as_array().unwrap().iter().enumerate() {
        if let Some(before) = case["before"].as_object() {
            state.clear();
            for (name, text) in before {
                fs::write(cwd.join(name), text.as_str().unwrap()).unwrap();
            }
        }
        let name = case["name"].as_str().unwrap();
        let selected_registry = if case["autoResizeImages"] == true {
            &resized_registry
        } else {
            &registry
        };
        let actual = run(selected_registry, &ctx, name, case["input"].clone()).await;
        if let Some(error) = case["error"].as_str() {
            assert_eq!(actual.unwrap_err().0, error, "case {i}: {}", case["input"]);
        } else {
            assert_eq!(
                fixture_output(actual.unwrap()),
                case["output"],
                "case {i}: {name} {}",
                case["input"]
            );
            if let Some(bytes) = case["bytes"].as_str() {
                assert_eq!(
                    fs::read(cwd.join(case["input"]["path"].as_str().unwrap())).unwrap(),
                    STANDARD.decode(bytes).unwrap()
                );
            }
        }
    }
}

#[tokio::test]
async fn registration_and_concurrency_are_explicit() {
    let mut registry = ToolRegistry::default();
    register(&mut registry).unwrap();
    assert_eq!(registry.specs().len(), 5);
    for name in ["read", "grep", "find"] {
        assert_eq!(
            registry.get(name).unwrap().concurrency(&json!({})),
            Concurrency::Shared
        );
    }
    for name in ["write", "edit"] {
        assert_eq!(
            registry.get(name).unwrap().concurrency(&json!({})),
            Concurrency::Exclusive
        );
    }
    assert!(register(&mut registry).is_err());
}

#[tokio::test]
async fn observed_recovery_is_fresh_unique_visible_and_literal() {
    let directory = tempfile::tempdir().unwrap();
    let cwd = directory.path();
    let path = cwd.join("a");
    let mut registry = ToolRegistry::default();
    let state = register(&mut registry).unwrap();
    let ctx = context(cwd);
    for old in [
        "1\tfirst\n2\t\t中文",
        "1: first\n2: \t中文",
        "first\\n\\t中文",
        "first\\n\\t\\u4e2d\\u6587",
    ] {
        state.clear();
        fs::write(&path, "\u{feff}first\r\n\t中文\r\nlast").unwrap();
        let input =
            json!({"path":"a","edits":[{"oldText":old,"newText":"const s = \"\\n\";\nchanged"}]});
        assert!(
            run(&registry, &ctx, "edit", input.clone())
                .await
                .unwrap_err()
                .0
                .contains("Could not find")
        );
        run(&registry, &ctx, "read", json!({"path":"a"}))
            .await
            .unwrap();
        assert!(
            text(&run(&registry, &ctx, "edit", input).await.unwrap())
                .contains("Recovered oldText representation")
        );
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "\u{feff}const s = \"\\n\";\r\nchanged\r\nlast"
        );
    }
    state.clear();
    fs::write(&path, "visible\nhidden\nend\n").unwrap();
    run(&registry, &ctx, "read", json!({"path":"a","limit":1}))
        .await
        .unwrap();
    assert!(
        run(
            &registry,
            &ctx,
            "edit",
            json!({"path":"a","edits":[{"oldText":"2\thidden","newText":"bad"}]})
        )
        .await
        .is_err()
    );
    run(&registry, &ctx, "read", json!({"path":"a"}))
        .await
        .unwrap();
    let error=run(&registry,&ctx,"edit",json!({"path":"a","edits":[{"oldText":"1\tvisible","newText":"a"},{"oldText":"visible\nhidden","newText":"b"}]})).await.unwrap_err();
    assert!(error.0.contains("overlap"));
    assert_eq!(fs::read_to_string(&path).unwrap(), "visible\nhidden\nend\n");
    state.retain_visible_reads(&Default::default());
    assert!(
        run(
            &registry,
            &ctx,
            "edit",
            json!({"path":"a","edits":[{"oldText":"1\tvisible","newText":"bad"}]})
        )
        .await
        .is_err()
    );
    fs::write(&path, "other\n").unwrap();
    for name in ["edit", "write"] {
        assert!(
            run(
                &registry,
                &ctx,
                name,
                json!({"path":"a","content":"bad","edits":[{"oldText":"other","newText":"bad"}]})
            )
            .await
            .unwrap_err()
            .0
            .contains("changed since it was read")
        );
    }
    run(&registry, &ctx, "read", json!({"path":"a"}))
        .await
        .unwrap();
    run(
        &registry,
        &ctx,
        "edit",
        json!({"path":"a","edits":[{"oldText":"other","newText":"ours"}]}),
    )
    .await
    .unwrap();
    run(
        &registry,
        &ctx,
        "write",
        json!({"path":"a","content":"next"}),
    )
    .await
    .unwrap();
    run(
        &registry,
        &ctx,
        "edit",
        json!({"path":"a","edits":[{"oldText":"next","newText":"done"}]}),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn batch_failures_never_write_and_diagnostics_remain_bounded() {
    let directory = tempfile::tempdir().unwrap();
    let mut registry = ToolRegistry::default();
    let state = register(&mut registry).unwrap();
    let ctx = context(directory.path());
    let line = "\t\t\"description\": \"保留原文中的制表符、空格与长中文字符串。TCP 等非 HTTP 请求也可以使用。\",";
    let content = format!("\u{feff}header\r\n{line}\r\nfooter");
    fs::write(directory.path().join("a"), &content).unwrap();
    let error=run(&registry,&ctx,"edit",json!({"path":"a","edits":[{"oldText":"header","newText":"HEADER"},{"oldText":line.replace("等非","等 非"),"newText":"bad"}]})).await.unwrap_err();
    for expected in [
        "No replacements from this call were written",
        "near file line 2",
        "Submitted:",
        "TCP 等 非 HTTP",
        "TCP 等非 HTTP",
        "\"offset\":1,\"limit\":8",
    ] {
        assert!(error.0.contains(expected), "{}", error.0);
    }
    assert!(error.0.len() < 2000);
    assert_eq!(
        fs::read_to_string(directory.path().join("a")).unwrap(),
        content
    );
    for content in [
        b"one\0rest".to_vec(),
        vec![255, 97],
        vec![111, 0, 110, 0, 101, 0],
    ] {
        state.clear();
        fs::write(directory.path().join("a"), &content).unwrap();
        assert!(
            run(
                &registry,
                &ctx,
                "edit",
                json!({"path":"a","edits":[{"oldText":"one","newText":"ONE"}]})
            )
            .await
            .unwrap_err()
            .0
            .contains("losslessly")
        );
        assert_eq!(fs::read(directory.path().join("a")).unwrap(), content);
    }
}

#[tokio::test]
async fn unicode_windows_continue_without_gaps() {
    let directory = tempfile::tempdir().unwrap();
    let mut registry = ToolRegistry::default();
    register(&mut registry).unwrap();
    let ctx = context(directory.path());
    let content = "中文😀".repeat(8000);
    fs::write(directory.path().join("a"), format!("{content}\r\nlast")).unwrap();
    let mut input = json!({"path":"a"});
    let mut combined = String::new();
    loop {
        let output = run(&registry, &ctx, "read", input).await.unwrap();
        combined += text(&output).split("\n\n[Line ").next().unwrap();
        let next = output.details.unwrap()["characterWindow"]["next"].clone();
        if next["offset"] == 2 {
            assert_eq!(
                text(&run(&registry, &ctx, "read", next).await.unwrap()),
                "last"
            );
            break;
        }
        input = next;
    }
    assert_eq!(combined, content);
    for input in [
        json!({"path":"a","column":0}),
        json!({"path":"a","column":1,"limit":2}),
    ] {
        assert!(
            run(&registry, &ctx, "read", input)
                .await
                .unwrap_err()
                .0
                .contains("Character windows")
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn symlinks_hardlinks_executable_bits_and_queues_preserve_the_file() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
    let directory = tempfile::tempdir().unwrap();
    let cwd = directory.path();
    let ctx = context(cwd);
    let mut registry = ToolRegistry::default();
    let state = register(&mut registry).unwrap();
    fs::write(cwd.join("a"), "one\ntwo\n").unwrap();
    fs::set_permissions(cwd.join("a"), fs::Permissions::from_mode(0o750)).unwrap();
    symlink("a", cwd.join("sym")).unwrap();
    run(
        &registry,
        &ctx,
        "edit",
        json!({"path":"sym","edits":[{"oldText":"one","newText":"ONE"}]}),
    )
    .await
    .unwrap();
    assert!(fs::symlink_metadata(cwd.join("sym")).unwrap().is_symlink());
    assert_eq!(fs::metadata(cwd.join("a")).unwrap().mode() & 0o777, 0o750);
    fs::hard_link(cwd.join("a"), cwd.join("hard")).unwrap();
    let ino = fs::metadata(cwd.join("a")).unwrap().ino();
    run(
        &registry,
        &ctx,
        "write",
        json!({"path":"a","content":"one\ntwo\n"}),
    )
    .await
    .unwrap();
    assert_eq!(fs::metadata(cwd.join("a")).unwrap().ino(), ino);
    assert_eq!(fs::read_to_string(cwd.join("hard")).unwrap(), "one\ntwo\n");
    let (a, b) = tokio::join!(
        run(
            &registry,
            &ctx,
            "edit",
            json!({"path":"a","edits":[{"oldText":"one","newText":"ONE"}]})
        ),
        run(
            &registry,
            &ctx,
            "edit",
            json!({"path":"a","edits":[{"oldText":"two","newText":"TWO"}]})
        )
    );
    a.unwrap();
    b.unwrap();
    assert_eq!(fs::read_to_string(cwd.join("a")).unwrap(), "ONE\nTWO\n");
    state.clear();
    run(&registry, &ctx, "read", json!({"path":"sym"}))
        .await
        .unwrap();
    fs::write(cwd.join("other"), "ONE\nTWO\n").unwrap();
    fs::remove_file(cwd.join("sym")).unwrap();
    symlink("other", cwd.join("sym")).unwrap();
    assert!(
        run(
            &registry,
            &ctx,
            "write",
            json!({"path":"sym","content":"bad"})
        )
        .await
        .unwrap_err()
        .0
        .contains("changed since it was read")
    );
    symlink("absent", cwd.join("dangling")).unwrap();
    assert!(
        run(
            &registry,
            &ctx,
            "write",
            json!({"path":"dangling","content":"bad"})
        )
        .await
        .is_err()
    );
    assert!(
        fs::symlink_metadata(cwd.join("dangling"))
            .unwrap()
            .is_symlink()
    );
}

#[tokio::test]
async fn cancellation_before_execution_never_writes() {
    let directory = tempfile::tempdir().unwrap();
    let mut registry = ToolRegistry::default();
    register(&mut registry).unwrap();
    let ctx = context(directory.path());
    ctx.cancel.cancel();
    for name in ["read", "grep", "find", "write", "edit"] {
        assert_eq!(
            run(&registry, &ctx, name, json!({"path":"a","content":"bad"}))
                .await
                .unwrap_err()
                .0,
            "Operation aborted"
        );
    }
    assert!(!directory.path().join("a").exists());
}

#[tokio::test]
async fn search_pagination_filters_and_unicode_match_offsets_follow_rg() {
    let directory = tempfile::tempdir().unwrap();
    let cwd = directory.path();
    let mut registry = ToolRegistry::default();
    register(&mut registry).unwrap();
    let ctx = context(cwd);
    fs::create_dir(cwd.join(".git")).unwrap();
    fs::write(cwd.join(".gitignore"), "ignored.ts\n").unwrap();
    for name in ["c.ts", "a.ts", "b.ts", "ignored.ts", "d.py"] {
        fs::write(cwd.join(name), "needle needle\nother\nneedle\n").unwrap();
    }
    for mode in ["files_with_matches", "count", "content"] {
        let pages = if mode == "content" { 6 } else { 3 };
        let mut rows = Vec::new();
        for offset in 0..pages {
            let result = run(
                &registry,
                &ctx,
                "grep",
                json!({"pattern":"needle","type":"ts","outputMode":mode,"offset":offset,"limit":1}),
            )
            .await
            .unwrap();
            assert_eq!(result.details.as_ref().unwrap()["nextOffset"], offset + 1);
            rows.push(text(&result).lines().next().unwrap().to_owned());
        }
        let expected: Vec<_> = ["a.ts", "b.ts", "c.ts"]
            .into_iter()
            .flat_map(|name| match mode {
                "content" => vec![
                    format!("{name}:1: needle needle"),
                    format!("{name}:3: needle"),
                ],
                "count" => vec![format!("{name}: 3")],
                _ => vec![name.into()],
            })
            .collect();
        assert_eq!(rows, expected);
        assert_eq!(text(&run(&registry,&ctx,"grep",json!({"pattern":"needle","type":"ts","outputMode":mode,"offset":pages,"limit":1})).await.unwrap()),format!("No more matches at offset={pages}"));
    }
    fs::write(cwd.join("unicode.txt"), "😀中文 hit\nnext hit\n").unwrap();
    assert_eq!(text(&run(&registry,&ctx,"grep",json!({"path":"unicode.txt","pattern":"hit\\nnext hit","multiline":true,"onlyMatching":true})).await.unwrap()),"unicode.txt:1: hit\nunicode.txt:2: next hit");
    let result = run(&registry, &ctx, "grep", json!({"pattern":"["}))
        .await
        .unwrap_err();
    assert!(result.0.contains("regex parse error"));
    for input in [
        json!({"offset":-1}),
        json!({"beforeContext":-1}),
        json!({"afterContext":0.5}),
    ] {
        let mut input = input;
        input["pattern"] = json!("needle");
        assert!(
            run(&registry, &ctx, "grep", input)
                .await
                .unwrap_err()
                .0
                .contains("safe integer")
        );
    }
}

#[tokio::test]
async fn find_keeps_hidden_gitignore_and_nested_path_globs() {
    let directory = tempfile::tempdir().unwrap();
    let cwd = directory.path();
    let mut registry = ToolRegistry::default();
    register(&mut registry).unwrap();
    let ctx = context(cwd);
    fs::create_dir(cwd.join(".git")).unwrap();
    fs::create_dir_all(cwd.join("nested/.git")).unwrap();
    fs::create_dir_all(cwd.join("nested/src")).unwrap();
    fs::create_dir(cwd.join("src")).unwrap();
    fs::write(cwd.join(".gitignore"), "ignored.ts\n").unwrap();
    for name in [
        "a.ts",
        ".hidden.ts",
        "ignored.ts",
        "src/a.spec.ts",
        "nested/src/a.spec.ts",
        "nested/ignored.ts",
        "b.txt",
    ] {
        fs::write(cwd.join(name), "").unwrap();
    }
    let output = run(&registry, &ctx, "find", json!({"pattern":"*.ts"}))
        .await
        .unwrap();
    let rows: Vec<_> = text(&output).lines().collect();
    assert!(rows.contains(&"a.ts") && rows.contains(&".hidden.ts"));
    assert!(!rows.contains(&"ignored.ts"));
    let output = run(
        &registry,
        &ctx,
        "find",
        json!({"pattern":"src/**/*.spec.ts"}),
    )
    .await
    .unwrap();
    let mut rows: Vec<_> = text(&output).lines().collect();
    rows.sort();
    assert_eq!(rows, ["src/a.spec.ts"]);
    let output = run(
        &registry,
        &ctx,
        "find",
        json!({"pattern":"**/src/**/*.spec.ts"}),
    )
    .await
    .unwrap();
    assert_eq!(text(&output), "nested/src/a.spec.ts\nsrc/a.spec.ts");
    assert!(
        run(&registry, &ctx, "find", json!({"pattern":"["}))
            .await
            .is_err()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn read_paths_match_cwd_at_prefix_and_decomposed_filenames() {
    use std::os::unix::fs::symlink;
    let directory = tempfile::tempdir().unwrap();
    let cwd = directory.path();
    let mut registry = ToolRegistry::default();
    register(&mut registry).unwrap();
    let ctx = context(cwd);
    fs::write(cwd.join("cafe\u{301}.txt"), "decomposed").unwrap();
    symlink("cafe\u{301}.txt", cwd.join("link.txt")).unwrap();
    for name in ["@café.txt", "link.txt"] {
        assert_eq!(
            text(
                &run(&registry, &ctx, "read", json!({"path":name}))
                    .await
                    .unwrap()
            ),
            "decomposed"
        );
    }
}

#[tokio::test]
async fn active_selection_and_observations_are_private_to_each_actor() {
    use tiechui_plugin_api::{Module, ModuleConfig, Registry};
    let directory = tempfile::tempdir().unwrap();
    let ctx = context(directory.path());
    let path = directory.path().join("shared.txt");
    fs::write(&path, "before").unwrap();
    let mut main = Registry::default();
    let mut child = Registry::default();
    main.restrict_tools(vec!["read".into(), "edit".into()]);
    tiechui_module_editing::EditingModule
        .register(&mut main, &ModuleConfig::default())
        .unwrap();
    tiechui_module_editing::EditingModule
        .register_child(&mut child, &ModuleConfig::default())
        .unwrap();
    child.restrict_tools(vec!["edit".into()]);
    assert_eq!(
        main.tools()
            .specs()
            .iter()
            .map(|s| s.name.as_str())
            .collect::<Vec<_>>(),
        ["edit", "read"]
    );
    assert_eq!(
        child
            .tools()
            .specs()
            .iter()
            .map(|s| s.name.as_str())
            .collect::<Vec<_>>(),
        ["edit"]
    );
    let read = main.tools().get("read").unwrap();
    assert!(read.spec().input_schema["properties"]["column"].is_object());
    assert!(read.spec().input_schema["properties"]["maxChars"].is_object());
    run(main.tools(), &ctx, "read", json!({"path":"shared.txt"}))
        .await
        .unwrap();
    fs::write(&path, "changed outside").unwrap();
    let edit = json!({"path":"shared.txt", "oldText":"changed outside", "newText":"child edit"});
    assert!(
        run(main.tools(), &ctx, "edit", edit.clone())
            .await
            .unwrap_err()
            .0
            .contains("changed since it was read")
    );
    run(child.tools(), &ctx, "edit", edit).await.unwrap();
    assert_eq!(fs::read_to_string(path).unwrap(), "child edit");
}

#[test]
fn previews_show_the_file_a_write_or_edit_would_leave() {
    let directory = tempfile::tempdir().unwrap();
    let cwd = directory.path();
    fs::write(cwd.join("a.txt"), "one\ntwo\n").unwrap();
    let preview = |name: &str, arguments: Value| {
        tiechui_module_editing::preview(
            &ToolCall {
                id: "preview".into(),
                name: name.into(),
                arguments,
                opaque: None,
            },
            cwd,
        )
    };
    let edit = |old: &str| json!({"path":"a.txt","edits":[{"oldText":old,"newText":"2"}]});
    assert_eq!(
        preview("edit", edit("two")),
        Some(("one\ntwo\n".into(), "one\n2\n".into()))
    );
    assert_eq!(preview("edit", edit("three")), None);
    assert_eq!(
        preview("write", json!({"path":"a.txt","content":"new"})),
        Some(("one\ntwo\n".into(), "new".into()))
    );
    assert_eq!(
        preview("write", json!({"path":"b/c.txt","content":"new"})),
        Some((String::new(), "new".into()))
    );
    assert_eq!(
        preview(
            "edit",
            json!({"path":"b/c.txt","edits":[{"oldText":"x","newText":"y"}]})
        ),
        None
    );
    assert_eq!(preview("read", json!({"path":"a.txt"})), None);
    fs::write(cwd.join("big.txt"), "x".repeat(300 * 1024)).unwrap();
    assert_eq!(
        preview("write", json!({"path":"big.txt","content":""})),
        None
    );
}
