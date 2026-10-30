//! Fixture loading persists a non-dropping store without any paid call.
use std::process::Command;

#[test]
fn load_persists_and_keeps_previously_loaded_posts() {
    let dir = tempfile::tempdir().unwrap();
    for id in ["demo-1", "demo-2"] {
        let fixture = dir.path().join("input.jsonl");
        std::fs::write(&fixture, serde_json::json!({"post_id":id, "text":"synthetic public post", "url":"https://example.com/post"}).to_string()+"\n").unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_bookmarks-agent"))
            .args(["load", fixture.to_str().unwrap()])
            .env("ANCIENTOS_HOME", dir.path())
            .env_remove("ANCIENTOS_ALLOW_PAID")
            .output()
            .unwrap();
        assert!(output.status.success());
    }
    let store =
        bookmarks_agent::ingest::Store::load_from(&dir.path().join("bookmarks.jsonl")).unwrap();
    assert_eq!(store.by_id.len(), 2);
    assert!(store.by_id.contains_key("demo-1"));
    assert!(store.by_id.contains_key("demo-2"));
}
