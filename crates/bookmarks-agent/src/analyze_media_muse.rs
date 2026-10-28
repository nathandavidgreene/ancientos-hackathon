//! `muse exec` [`MediaBackend`]: runs the Muse Code CLI headless, one image
//! per call, because the Meta key only works through the CLI.
//!
//! Per item: the image is downloaded (X media hosts only, size-capped) into
//! a fresh temp dir as `image.<ext>` (neutral name, no URL in the prompt),
//! then `muse exec --json --image <file> --workspace <tmp> --trust-workspace
//! --reasoning-effort low <prompt>` runs with that dir as the only trusted
//! workspace. The reply is the `text` of the `run.terminal.completed` event,
//! parsed by [`parse_findings`]; `run.usage` input+output tokens add to a
//! running total. Once the total reaches `ANCIENTOS_MEDIA_MAX_TOKENS`, every
//! further item fails before any call (fail closed). Construction needs a
//! [`PaidConfig`], the token cap, and a `muse` binary.
use crate::analyze_media::{MediaBackend, PaidConfig, RawFinding, ResolvedMedia};
use crate::analyze_media_live::{parse_findings, PROMPT};
use std::cell::Cell;
use std::path::{Path, PathBuf};

/// Per-run token cap (input + output, summed over calls); required.
pub const MAX_TOKENS_ENV_VAR: &str = "ANCIENTOS_MEDIA_MAX_TOKENS";
/// Optional path to the `muse` binary; default `muse` on `PATH`.
pub const MUSE_BIN_ENV_VAR: &str = "ANCIENTOS_MUSE_BIN";
/// Largest image accepted, in bytes.
pub const MAX_IMAGE_BYTES: u64 = 10 * 1024 * 1024;
/// Prompt suffix telling the agent not to use tools.
pub const NO_TOOLS: &str = " Look only at the attached image. Do not use any tools.";

/// Token usage from one `run.usage` event.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
}

/// Downloads an image URL to a path.
pub type Fetch = Box<dyn Fn(&str, &Path) -> Result<(), String>>;
/// Runs `muse` with args in a dir and returns stdout.
pub type Exec = Box<dyn Fn(&[String], &Path) -> Result<String, String>>;

/// The `muse exec` backend with a running token total.
pub struct MuseExecBackend {
    muse_bin: String,
    max_tokens: u64,
    used: Cell<Usage>,
    calls: Cell<u64>,
    fetch: Fetch,
    exec: Exec,
}

/// Parse the token cap; must be a positive integer.
pub fn parse_token_cap(v: Option<&str>) -> Result<u64, String> {
    match v.map(str::trim).map(str::parse::<u64>) {
        Some(Ok(n)) if n > 0 => Ok(n),
        _ => Err(format!(
            "set {MAX_TOKENS_ENV_VAR} to a positive token cap (fail closed)"
        )),
    }
}

/// Build the backend from env with real download and process execution.
pub fn muse_backend(
    env: &dyn Fn(&str) -> Option<String>,
    paid: PaidConfig,
) -> Result<MuseExecBackend, String> {
    let bin = env(MUSE_BIN_ENV_VAR)
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "muse".into());
    let ok = std::process::Command::new(&bin)
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !ok {
        return Err(format!("muse binary '{bin}' not runnable (fail closed)"));
    }
    MuseExecBackend::with_io(env, paid, bin, Box::new(http_fetch), Box::new(run_process))
}

impl MuseExecBackend {
    /// Build with injected download/exec (tests); checks the token cap.
    pub fn with_io(
        env: &dyn Fn(&str) -> Option<String>,
        _paid: PaidConfig,
        muse_bin: String,
        fetch: Fetch,
        exec: Exec,
    ) -> Result<Self, String> {
        Ok(Self {
            muse_bin,
            max_tokens: parse_token_cap(env(MAX_TOKENS_ENV_VAR).as_deref())?,
            used: Cell::new(Usage::default()),
            calls: Cell::new(0),
            fetch,
            exec,
        })
    }

    /// Tokens used so far in this run.
    pub fn used(&self) -> Usage {
        self.used.get()
    }

    /// Calls made so far in this run.
    pub fn calls(&self) -> u64 {
        self.calls.get()
    }

    /// Arguments for one `muse exec` call on `image` inside `dir`.
    pub fn args_for(&self, image: &Path, dir: &Path) -> Vec<String> {
        vec![
            "exec".into(),
            "--json".into(),
            "--reasoning-effort".into(),
            "low".into(),
            "--contributor-mode".into(),
            "off".into(),
            "--workspace".into(),
            dir.display().to_string(),
            "--trust-workspace".into(),
            "--image".into(),
            image.display().to_string(),
            format!("{PROMPT}{NO_TOOLS}"),
        ]
    }
}

/// Neutral file extension from the URL path (`jpg` unless png/webp/gif).
fn ext_for(url: &str) -> &'static str {
    let path = url
        .split(['?', '#'])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    if path.ends_with(".png") {
        "png"
    } else if path.ends_with(".webp") {
        "webp"
    } else if path.ends_with(".gif") {
        "gif"
    } else {
        "jpg"
    }
}

/// Parse `muse exec --json` stdout: the final reply text and usage.
/// Missing terminal event, a non-completed terminal, or a run that used
/// tools is an error.
pub fn parse_events(stdout: &str) -> Result<(String, Usage), String> {
    let mut text = None;
    let mut usage = Usage::default();
    for line in stdout.lines().filter(|l| !l.trim().is_empty()) {
        let e: serde_json::Value =
            serde_json::from_str(line).map_err(|e| format!("bad event: {e}"))?;
        let kind = e["payload_type"].as_str().unwrap_or("");
        let p = &e["payload"];
        if kind.starts_with("tool.") {
            return Err("model tried to use a tool".into());
        }
        if kind == "run.usage" {
            for m in p["models"].as_array().into_iter().flatten() {
                usage.input += m["input_tokens"].as_u64().unwrap_or(0);
                usage.output += m["output_tokens"].as_u64().unwrap_or(0);
            }
        }
        if kind.starts_with("run.terminal.") {
            if p["terminal"] != "completed" {
                return Err(format!("run ended {}", p["terminal"]));
            }
            text = p["text"].as_str().map(String::from);
        }
    }
    Ok((text.ok_or("no run.terminal.completed text")?, usage))
}

impl MediaBackend for MuseExecBackend {
    /// Download, run one `muse exec`, add usage, parse findings.
    fn analyze(&self, item: &ResolvedMedia) -> Result<Vec<RawFinding>, String> {
        let before = self.used.get();
        if before.input + before.output >= self.max_tokens {
            return Err(format!("token cap {} reached", self.max_tokens));
        }
        let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
        let image: PathBuf = dir
            .path()
            .join(format!("image.{}", ext_for(&item.provenance.media_url)));
        (self.fetch)(&item.provenance.media_url, &image)?;
        let mut args = self.args_for(&image, dir.path());
        args.insert(0, self.muse_bin.clone());
        let stdout = (self.exec)(&args, dir.path())?;
        self.calls.set(self.calls.get() + 1);
        let parsed = parse_events(&stdout);
        if let Ok((_, u)) = &parsed {
            self.used.set(Usage {
                input: before.input + u.input,
                output: before.output + u.output,
            });
        }
        parse_findings(&parsed?.0)
    }

    /// Calls and tokens used so far.
    fn usage_summary(&self) -> Option<String> {
        let u = self.used.get();
        Some(format!(
            "{} call(s), {} input + {} output tokens",
            self.calls.get(),
            u.input,
            u.output
        ))
    }
}

/// Download an X media URL to `path`, refusing non-X hosts and big files.
fn http_fetch(url: &str, path: &Path) -> Result<(), String> {
    let url = crate::media::safe_media_url(url).ok_or("unsafe media URL")?;
    let resp = ureq::get(&url)
        .timeout(std::time::Duration::from_secs(30))
        .call()
        .map_err(|e| format!("download failed: {e}"))?;
    let mut buf = Vec::new();
    use std::io::Read;
    resp.into_reader()
        .take(MAX_IMAGE_BYTES + 1)
        .read_to_end(&mut buf)
        .map_err(|e| e.to_string())?;
    if buf.len() as u64 > MAX_IMAGE_BYTES {
        return Err("image too large".into());
    }
    std::fs::write(path, buf).map_err(|e| e.to_string())
}

/// Run `args[0]` with the rest in `dir`, returning stdout; nonzero exit errors.
fn run_process(args: &[String], dir: &Path) -> Result<String, String> {
    let (bin, rest) = args.split_first().ok_or("empty command")?;
    let out = std::process::Command::new(bin)
        .args(rest)
        .current_dir(dir)
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(format!("muse exited {}", out.status));
    }
    String::from_utf8(out.stdout).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyze_media::Provenance;

    /// Recorded-shape event stream (synthetic text, real event layout).
    const EVENTS: &str = concat!(
        r#"{"payload_type":"run.lifecycle.started","payload":{}}"#,
        "\n",
        r#"{"payload_type":"run.usage","payload":{"models":[{"input_tokens":28947,"output_tokens":442,"reasoning_tokens":340}]}}"#,
        "\n",
        r#"{"payload_type":"run.terminal.completed","payload":{"terminal":"completed","text":"{\"findings\":[{\"kind\":\"text\",\"text\":\"[synthetic] OPEN\",\"quote\":\"OPEN\",\"region\":null}]}"}}"#,
        "\n"
    );

    /// Test helper: env with a token cap.
    fn env_cap(cap: &'static str) -> impl Fn(&str) -> Option<String> {
        move |k| (k == MAX_TOKENS_ENV_VAR).then(|| cap.to_string())
    }

    /// Test helper: backend whose exec returns `out`.
    fn backend(cap: &'static str, out: &'static str) -> MuseExecBackend {
        MuseExecBackend::with_io(
            &env_cap(cap),
            PaidConfig { max_items: 5 },
            "muse".into(),
            Box::new(|_, p| std::fs::write(p, b"x").map_err(|e| e.to_string())),
            Box::new(move |args, _| {
                assert!(args.contains(&"--trust-workspace".to_string()));
                assert!(
                    !args.iter().any(|a| a.contains("twimg")),
                    "URL leaked into args"
                );
                Ok(out.to_string())
            }),
        )
        .unwrap()
    }

    /// Test helper: one resolved item.
    fn item() -> ResolvedMedia {
        ResolvedMedia {
            provenance: Provenance {
                post_id: "demo-1".into(),
                media_key: "3_k".into(),
                media_url: "https://pbs.twimg.com/media/demo.png".into(),
                frame_idx: None,
            },
            kind: "photo".into(),
            alt_text: None,
        }
    }

    #[test]
    fn parses_recorded_events_and_counts_tokens() {
        let (text, u) = parse_events(EVENTS).unwrap();
        assert!(text.contains("findings"));
        assert_eq!(
            u,
            Usage {
                input: 28947,
                output: 442
            }
        );
        let b = backend("1000000", EVENTS);
        assert_eq!(b.analyze(&item()).unwrap().len(), 1);
        assert_eq!(b.used().input, 28947);
        assert_eq!(b.calls(), 1);
    }

    #[test]
    fn token_cap_stops_before_next_call() {
        let b = backend("1000", EVENTS);
        assert!(b.analyze(&item()).is_ok());
        assert!(b.analyze(&item()).unwrap_err().contains("cap"));
        assert_eq!(b.calls(), 1);
    }

    #[test]
    fn missing_cap_fails_closed() {
        assert!(parse_token_cap(None).is_err());
        assert!(parse_token_cap(Some("0")).is_err());
        assert!(parse_token_cap(Some("abc")).is_err());
        assert_eq!(parse_token_cap(Some("5")), Ok(5));
    }

    #[test]
    fn bad_streams_are_errors() {
        assert!(parse_events(r#"{"payload_type":"run.usage","payload":{}}"#).is_err());
        assert!(parse_events(
            r#"{"payload_type":"run.terminal.failed","payload":{"terminal":"failed"}}"#
        )
        .is_err());
        assert!(parse_events(r#"{"payload_type":"tool.call.started","payload":{}}"#).is_err());
        assert!(parse_events("not json").is_err());
        let b = backend(
            "1000000",
            r#"{"payload_type":"run.terminal.completed","payload":{"terminal":"completed","text":"I think it shows a chart"}}"#,
        );
        assert!(b.analyze(&item()).is_err());
    }

    #[test]
    fn neutral_names_and_low_effort() {
        assert_eq!(ext_for("https://pbs.twimg.com/media/a.PNG?x=1"), "png");
        assert_eq!(ext_for("https://pbs.twimg.com/media/a"), "jpg");
        let b = backend("10", EVENTS);
        let a = b.args_for(Path::new("/t/image.jpg"), Path::new("/t"));
        assert!(a
            .windows(2)
            .any(|w| w[0] == "--reasoning-effort" && w[1] == "low"));
    }
}
