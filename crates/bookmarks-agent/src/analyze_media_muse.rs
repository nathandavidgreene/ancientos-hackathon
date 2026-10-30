//! `muse exec` [`MediaBackend`]: runs the Muse Code CLI headless, one image
//! per call, because the Meta key only works through the CLI.
//!
//! Per item: the image is downloaded (X media hosts only, size-capped) into
//! a fresh temp dir as `image.<ext>` (neutral name, no URL in the prompt),
//! then `muse exec --json --image <file> --workspace <tmp>` with shell,
//! filesystem writes, web tools and reminder agents disabled, and a 120s deadline.
//! The command uses
//! `--reasoning-effort low <prompt>` with that directory as its isolated
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
    paid: PaidConfig,
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
        paid: PaidConfig,
        muse_bin: String,
        fetch: Fetch,
        exec: Exec,
    ) -> Result<Self, String> {
        Ok(Self {
            paid,
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
            "--disable-shell".into(),
            "--disable-write".into(),
            "--disable-web-tools".into(),
            "--disable-reminders".into(),
            "--disable-request-user-input".into(),
            "--no-foreign-personal-context".into(),
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
        self.paid.require_current()?;
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
    use ancientos_router::http::HttpError;
    let url = crate::media::safe_media_url(url).ok_or("unsafe media URL")?;
    let fetched = ancientos_router::http::get_bytes(
        &url,
        std::time::Duration::from_secs(30),
        Some(0),
        MAX_IMAGE_BYTES + 1,
    )
    .map_err(|e| match e {
        HttpError::Status { code, .. } => format!("download failed: http status {code}"),
        HttpError::Transport { message, .. } => format!("download failed: {message}"),
        HttpError::Body(e) => e.to_string(),
    })?;
    crate::media::safe_media_url(&fetched.final_url).ok_or("unsafe final media URL")?;
    if fetched.bytes.len() as u64 > MAX_IMAGE_BYTES {
        return Err("image too large".into());
    }
    std::fs::write(path, fetched.bytes).map_err(|e| e.to_string())
}

/// Run `args[0]` with the rest in `dir`, returning stdout; nonzero exit errors.
fn run_process(args: &[String], dir: &Path) -> Result<String, String> {
    run_process_with_timeout(args, dir, std::time::Duration::from_secs(120))
}

/// Run with a bounded deadline, killing and reaping the child on timeout.
/// Stdout is spooled to a private file so a full pipe cannot deadlock wait.
fn run_process_with_timeout(
    args: &[String],
    dir: &Path,
    timeout: std::time::Duration,
) -> Result<String, String> {
    use std::io::{Read, Seek};
    let (bin, rest) = args.split_first().ok_or("empty command")?;
    let mut stdout = tempfile::tempfile().map_err(|_| "stdout staging failed".to_string())?;
    let mut child = std::process::Command::new(bin)
        .args(rest)
        .current_dir(dir)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .stdout(
            stdout
                .try_clone()
                .map_err(|_| "stdout staging failed".to_string())?,
        )
        .spawn()
        .map_err(|_| "muse spawn failed".to_string())?;
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return Err(format!("muse exited {status}"));
                }
                break;
            }
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("muse deadline or wait failure".into());
            }
        }
    }
    stdout
        .rewind()
        .map_err(|_| "stdout read failed".to_string())?;
    let mut bytes = Vec::new();
    stdout
        .take(4 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "stdout read failed".to_string())?;
    if bytes.len() > 4 * 1024 * 1024 {
        return Err("muse output exceeds cap".into());
    }
    String::from_utf8(bytes).map_err(|_| "invalid muse output".to_string())
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
                assert!(args.contains(&"--disable-shell".to_string()));
                assert!(!args.contains(&"--trust-workspace".to_string()));
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
    fn process_deadline_kills_hung_child() {
        let dir = tempfile::tempdir().unwrap();
        let start = std::time::Instant::now();
        let args = vec!["/bin/sh".into(), "-c".into(), "sleep 2".into()];
        assert!(
            run_process_with_timeout(&args, dir.path(), std::time::Duration::from_millis(20))
                .is_err()
        );
        assert!(start.elapsed() < std::time::Duration::from_secs(1));
    }

    #[test]
    fn paid_revocation_precedes_download_and_execution() {
        let _paid = crate::test_support::EnvHold::paid_on();
        let b = MuseExecBackend::with_io(
            &env_cap("100"),
            PaidConfig { max_items: 1 },
            "muse".into(),
            Box::new(|_, _| panic!("download after revocation")),
            Box::new(|_, _| panic!("execution after revocation")),
        )
        .unwrap();
        std::env::remove_var(crate::analyze_media::PAID_ENV_VAR);
        assert!(b.analyze(&item()).unwrap_err().contains("disabled"));
        assert_eq!(b.calls(), 0);
    }

    #[test]
    fn parses_recorded_events_and_counts_tokens() {
        let _paid = crate::test_support::EnvHold::paid_on();
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
        let _paid = crate::test_support::EnvHold::paid_on();
        let b = backend("1000", EVENTS);
        assert!(b.analyze(&item()).is_ok());
        assert!(b.analyze(&item()).unwrap_err().contains("cap"));
        assert_eq!(b.calls(), 1);
    }

    #[test]
    fn missing_cap_fails_closed() {
        let _paid = crate::test_support::EnvHold::paid_on();
        assert!(parse_token_cap(None).is_err());
        assert!(parse_token_cap(Some("0")).is_err());
        assert!(parse_token_cap(Some("abc")).is_err());
        assert_eq!(parse_token_cap(Some("5")), Ok(5));
    }

    #[test]
    fn bad_streams_are_errors() {
        let _paid = crate::test_support::EnvHold::paid_on();
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
    fn metadata_download_is_refused_without_writing() {
        let _paid = crate::test_support::EnvHold::paid_on();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("image");
        assert!(http_fetch("http://169.254.169.254/latest/meta-data/", &path).is_err());
        assert!(!path.exists());
    }

    #[test]
    fn neutral_names_and_low_effort() {
        let _paid = crate::test_support::EnvHold::paid_on();
        assert_eq!(ext_for("https://pbs.twimg.com/media/a.PNG?x=1"), "png");
        assert_eq!(ext_for("https://pbs.twimg.com/media/a"), "jpg");
        let b = backend("10", EVENTS);
        let a = b.args_for(Path::new("/t/image.jpg"), Path::new("/t"));
        assert!(a
            .windows(2)
            .any(|w| w[0] == "--reasoning-effort" && w[1] == "low"));
    }
}
