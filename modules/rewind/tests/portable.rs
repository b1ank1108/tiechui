#![allow(clippy::unwrap_used)]

use std::fs;
use tiechui_module_rewind::store::Store;
use tiechui_plugin_api::CancellationToken;

#[test]
fn tracked_edit_can_be_restored_and_settled() {
    let root = tempfile::tempdir().unwrap();
    let work = root.path().join("work");
    fs::create_dir(&work).unwrap();
    let store = Store::new(&root.path().join("state"), &work, "session").unwrap();
    let cancel = CancellationToken::new();
    fs::write(work.join("a.txt"), "before\r\n").unwrap();
    store.before("turn", "a.txt", &cancel).unwrap();
    fs::write(work.join("a.txt"), "after\r\n").unwrap();
    store.after("a.txt", &cancel).unwrap();
    let plan = store.plan(&["turn".into()], &cancel).unwrap();
    let _guard = store.restore(&plan, &cancel).unwrap();
    assert!(store.pending().unwrap());
    store.finish(&plan.target).unwrap();
    assert!(!store.pending().unwrap());
    assert_eq!(fs::read(work.join("a.txt")).unwrap(), b"before\r\n");
}
