//! Label public bookmarks with Jev: stdin JSONL `{"id","text"}` -> stdout JSONL.
//! Sequential, stops at the first error (no retries). Paid-gated via JevClient.
use ancientos_eval::jev_label_bookmark;
use ancientos_router::{jev_client::JevClient, Sensitivity};
use std::io::{BufRead, Write};

/// Example: label sample bookmarks with Jev.
fn main() {
    let client = JevClient::from_env().expect("jev backend selection");
    let mut out = std::io::stdout().lock();
    for line in std::io::stdin().lock().lines() {
        let v: serde_json::Value = serde_json::from_str(&line.expect("stdin")).expect("json line");
        let text = v["text"].as_str().unwrap_or("");
        match jev_label_bookmark(&client, Sensitivity::Sanitized, text) {
            Ok(l) => {
                let row = serde_json::json!({"post_id": v["id"], "try_it_p": l.try_it,
                    "worth_resurfacing_p": l.worth_resurfacing, "category": l.category,
                    "input_tokens": l.input_tokens, "output_tokens": l.output_tokens});
                writeln!(out, "{row}").unwrap();
                out.flush().unwrap();
            }
            Err(e) => {
                eprintln!("STOP on {}: {e}", v["id"]);
                std::process::exit(2);
            }
        }
    }
}
