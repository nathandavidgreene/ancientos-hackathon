//! Muted roots never reach a thread fetch or model backend.
use bookmarks_agent::{deeper, delegate, topic_mute};

#[test]
fn deeper_and_delegate_prescreen_before_any_backend() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store.jsonl");
    std::fs::write(&store, serde_json::json!({"post_id":"demo-123", "text":"zorblax", "url":"https://example.com/post"}).to_string()+"\n").unwrap();
    let _mute = topic_mute::set_terms(topic_mute::MuteList::new(["zorblax"]));
    let log = dir.path().join("log.jsonl");
    let out = dir.path().join("out");
    let thread = |_: &str| -> Result<_, String> { panic!("muted root fetched") };
    let post = |_: &str| -> Result<_, String> { panic!("muted root fetched") };
    let deeper_call =
        |_: &str, _: Option<&str>| -> Result<_, String> { panic!("muted root modeled") };
    let delegate_call = |_: &str| -> Result<_, String> { panic!("muted root modeled") };
    assert!(deeper::run_with(
        "demo-123",
        true,
        None,
        &thread,
        &post,
        &deeper_call,
        &log,
        &out,
        None
    )
    .unwrap_err()
    .contains("excluded topic"));
    assert!(delegate::run_with(
        "demo-123",
        true,
        &thread,
        &delegate_call,
        &log,
        &out,
        "2026-10-30",
        None
    )
    .unwrap_err()
    .contains("excluded topic"));
    assert!(!log.exists());
    assert!(!out.exists());
}
