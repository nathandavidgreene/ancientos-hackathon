//! `web-ui --data DIR [--port 8787] [--host 127.0.0.1] [--demo]` —
//! loopback-only demo server. Thin binary: arg parsing plus demo setup;
//! serving and rendering live in the lib.
use std::net::TcpListener;
use web_ui::{loopback_addr, State, DEFAULT_PORT};

const USAGE: &str = "usage: web-ui --data DIR [--port 8787] [--host 127.0.0.1] [--demo]";
/// Same bookmark fixtures `scripts/demo.sh` syncs.
const DEMO_FIXTURES: &[&str] = &[
    "fixtures/demo/synthetic_bookmarks.jsonl",
    "fixtures/demo/try_it_and_thread_bookmarks.jsonl",
];
/// Synthetic idea cards seeded into the demo data dir when present.
const DEMO_CARDS_FIXTURE: &str = "fixtures/demo/expected_idea_cards.jsonl";

struct Args {
    data: Option<String>,
    port: u16,
    host: String,
    demo: bool,
}

fn parse_args(args: &[String]) -> Result<Args, String> {
    let mut out = Args {
        data: None,
        port: DEFAULT_PORT,
        host: "127.0.0.1".to_string(),
        demo: false,
    };
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--data" => {
                i += 1;
                out.data = Some(
                    args.get(i)
                        .filter(|s| !s.is_empty())
                        .ok_or(USAGE.to_string())?
                        .clone(),
                );
                i += 1;
            }
            "--port" => {
                i += 1;
                let raw = args.get(i).ok_or(USAGE.to_string())?;
                out.port = raw
                    .parse::<u16>()
                    .map_err(|_| format!("bad --port: {raw}"))?;
                i += 1;
            }
            "--host" => {
                i += 1;
                out.host = args
                    .get(i)
                    .filter(|s| !s.is_empty())
                    .ok_or(USAGE.to_string())?
                    .clone();
                i += 1;
            }
            "--demo" => {
                out.demo = true;
                i += 1;
            }
            _ => return Err(USAGE.into()),
        }
    }
    if !out.demo && out.data.is_none() {
        return Err(USAGE.into());
    }
    Ok(out)
}

/// Load the demo bookmark fixtures into a temp data dir, new rows only
/// (same incremental rule as `demo-runner sync`). Never touches real data.
fn setup_demo() -> Result<std::path::PathBuf, String> {
    let dir = std::env::temp_dir().join(format!("web-ui-demo-{}", std::process::id()));
    std::fs::create_dir_all(&dir).map_err(|e| format!("demo dir failed: {e}"))?;
    let store = dir.join("store.jsonl");
    let mut rows: Vec<bookmarks_agent::fixture::Bookmark> = match std::fs::read_to_string(&store) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(format!("demo store read failed: {e}")),
        Ok(text) => bookmarks_agent::fixture::parse_jsonl(&text)
            .map_err(|e| format!("demo store parse failed: {e}"))?,
    };
    let mut have: std::collections::BTreeSet<String> =
        rows.iter().map(|b| b.post_id.clone()).collect();
    for fix in DEMO_FIXTURES {
        let incoming = bookmarks_agent::fixture::load(fix)
            .map_err(|e| format!("demo fixture {fix} failed: {e}"))?;
        for b in incoming {
            if have.insert(b.post_id.clone()) {
                rows.push(b);
            }
        }
    }
    let body: Vec<String> = rows
        .iter()
        .map(serde_json::to_string)
        .collect::<Result<_, _>>()
        .map_err(|e| e.to_string())?;
    std::fs::write(&store, body.join("\n") + "\n")
        .map_err(|e| format!("demo store write failed: {e}"))?;
    let cards = dir.join("idea_cards.jsonl");
    if !cards.exists() {
        if let Ok(seed) = std::fs::read(DEMO_CARDS_FIXTURE) {
            std::fs::write(&cards, seed).map_err(|e| format!("demo cards write failed: {e}"))?;
        }
    }
    println!(
        "demo data dir: {} ({} bookmarks)",
        dir.display(),
        rows.len()
    );
    Ok(dir)
}

fn run(args: &[String]) -> Result<(), String> {
    let parsed = parse_args(args)?;
    let dir = if parsed.demo {
        setup_demo()?
    } else {
        parsed.data.clone().expect("checked in parse_args").into()
    };
    let state = State::open(&dir)?;
    let addr = loopback_addr(&parsed.host, parsed.port)?;
    let listener = TcpListener::bind(addr).map_err(|e| format!("bind {addr} failed: {e}"))?;
    println!(
        "Serving {} bookmarks on http://{addr}/ (loopback only)",
        state.bookmarks.len()
    );
    println!("Open that URL in a browser; press Ctrl-C to stop.");
    web_ui::serve(listener, &state, None);
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Err(e) = run(&args) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
