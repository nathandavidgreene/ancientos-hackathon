//! Append-only, hash-chained JSONL decision log.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Failures from opening, appending to, or verifying a [`DecisionLog`].
///
/// A [`LogError::Lock`] failure is fail-closed: nothing was written and the
/// caller's in-memory head is unchanged. Callers must surface the error, not
/// retry as an unlocked write.
#[derive(Debug, thiserror::Error)]
pub enum LogError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("chain broken at line {0}")]
    Chain(usize),
    /// The exclusive log lock could not be acquired, a lock syscall failed,
    /// or a guard for another log was passed: nothing was written.
    #[error("lock: {0}")]
    Lock(String),
}

/// Read-only load failure: either the file could not be read at all, or the
/// first broken line (1-based) and why it broke. Unlike
/// [`DecisionLog::open`], a missing file is an error: there is nothing to
/// inspect, and reporting "0 entries" would hide a typo'd path.
#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("cannot read log: {0}")]
    Io(#[from] std::io::Error),
    #[error("line {line}: {reason}")]
    Broken { line: usize, reason: String },
}

/// Build a broken-chain error for a 1-based line.
fn broken(line: usize, reason: String) -> LoadError {
    LoadError::Broken { line, reason }
}

/// Open `path` read-only, parse every line and check the hash chain,
/// reporting the first broken line (1-based) and reason. Never creates,
/// writes, or truncates the file: the only I/O is one read-only open.
/// Fail closed: an unreadable line, invalid JSON, a seq gap, a `prev_hash`
/// mismatch or a hash mismatch is an error naming that line.
pub fn load_verified(path: &Path) -> Result<Vec<Entry>, LoadError> {
    let file = File::open(path)?;
    let mut prev = GENESIS.to_string();
    let mut out = Vec::new();
    for (i, line) in BufReader::new(file).lines().enumerate() {
        let lineno = i + 1;
        let line = line.map_err(|e| broken(lineno, format!("unreadable line: {e}")))?;
        let e: Entry = serde_json::from_str(&line)
            .map_err(|e| broken(lineno, format!("invalid JSON: {e}")))?;
        if e.seq != i as u64 {
            return Err(broken(lineno, format!("expected seq {i}, found {}", e.seq)));
        }
        if e.prev_hash != prev {
            return Err(broken(
                lineno,
                "prev_hash does not match previous entry (tampered or corrupt)".to_string(),
            ));
        }
        if e.hash != digest(e.seq, &e.prev_hash, &e.kind, &e.payload) {
            return Err(broken(
                lineno,
                "hash mismatch (tampered or corrupt)".to_string(),
            ));
        }
        prev = e.hash.clone();
        out.push(e);
    }
    Ok(out)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Entry {
    pub seq: u64,
    pub prev_hash: String,
    pub kind: String,
    pub payload: serde_json::Value,
    pub hash: String,
}

const GENESIS: &str = "0";

/// Hash-chain digest of a log entry (seq, previous hash, kind, payload).
fn digest(seq: u64, prev: &str, kind: &str, payload: &serde_json::Value) -> String {
    let mut h = Sha256::new();
    h.update(format!("{seq}|{prev}|{kind}|{payload}"));
    hex::encode(h.finalize())
}

/// How long a writer waits for the exclusive log lock before failing closed.
const LOCK_TIMEOUT: Duration = Duration::from_secs(30);

/// Poll interval while waiting for the exclusive log lock.
const LOCK_POLL: Duration = Duration::from_millis(5);

/// Sibling lock file for a log path: `<path>.lock`, created on first use.
///
/// The lock lives beside the log (rather than on the log file itself) so its
/// lifecycle is independent of log creation and of the read-only opens in
/// [`DecisionLog::verify`]: only writers take it, always exclusively.
fn lock_path_for(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".lock");
    PathBuf::from(s)
}

/// Exclusive advisory (`flock`, `LOCK_EX`) lock on one decision log.
///
/// One guard covers one critical section (read-tail, hash, write, fsync).
/// The lock is cross-process: a second process waits (bounded, see
/// [`DecisionLog::acquire_lock`]) until the guard is dropped. Dropping
/// releases the lock; closing the handle releases it even if `LOCK_UN` fails.
#[derive(Debug)]
pub struct LogLock {
    /// Open handle to the sibling `.lock` file; the `flock` is tied to it.
    file: File,
    /// Which lock file this guard holds, so a mismatched guard fails closed.
    path: PathBuf,
}

impl Drop for LogLock {
    /// Release the advisory lock (best effort; close releases it anyway).
    fn drop(&mut self) {
        // SAFETY: `flock` on a live open fd has no memory effects. The
        // result is ignored: closing `file` right after releases the lock.
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

/// Acquire an exclusive advisory lock on `lock_path`, polling at most
/// `LOCK_TIMEOUT`. Any `flock` error or timeout fails closed with
/// [`LogError::Lock`]; there is no unlocked fallback.
fn acquire_exclusive(lock_path: &Path) -> Result<LogLock, LogError> {
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        // The lock file holds no data (only the flock); never truncate it.
        .truncate(false)
        .open(lock_path)
        .map_err(|e| LogError::Lock(format!("cannot open {}: {e}", lock_path.display())))?;
    let fd = file.as_raw_fd();
    let start = Instant::now();
    loop {
        // SAFETY: `flock` on a live open fd has no memory effects.
        let rc = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
        if rc == 0 {
            return Ok(LogLock {
                file,
                path: lock_path.to_path_buf(),
            });
        }
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::EWOULDBLOCK) {
            return Err(LogError::Lock(format!(
                "flock {}: {err}",
                lock_path.display()
            )));
        }
        if start.elapsed() >= LOCK_TIMEOUT {
            return Err(LogError::Lock(format!(
                "timed out waiting for {}",
                lock_path.display()
            )));
        }
        std::thread::sleep(LOCK_POLL);
    }
}

/// Append-only decision log: one hash-chained JSONL file plus its lock.
#[derive(Debug)]
pub struct DecisionLog {
    path: PathBuf,
    next_seq: u64,
    last_hash: String,
}

impl DecisionLog {
    /// Open (or create) a log, verifying the existing chain.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, LogError> {
        let path = path.as_ref().to_path_buf();
        let mut log = Self {
            path,
            next_seq: 0,
            last_hash: GENESIS.into(),
        };
        if log.path.exists() {
            for e in log.verify()? {
                log.next_seq = e.seq + 1;
                log.last_hash = e.hash;
            }
        }
        Ok(log)
    }

    /// Acquire this log's exclusive advisory lock (the sibling `.lock` file).
    ///
    /// Lets one writer hold a single critical section across several steps
    /// (see [`DecisionLog::append_locked`]). Fails closed with
    /// [`LogError::Lock`] on any lock error; never returns an unlocked guard.
    pub fn acquire_lock(&self) -> Result<LogLock, LogError> {
        acquire_exclusive(&lock_path_for(&self.path))
    }

    /// Append one entry. The file is only ever opened in append mode.
    ///
    /// Cross-process safe: holds the exclusive advisory lock for the whole
    /// read-tail / hash / write / fsync sequence and re-reads the on-disk
    /// head under the lock, so concurrent appenders form one unbroken hash
    /// chain instead of forking from a stale in-memory head. A lock failure
    /// or a broken on-disk chain fails closed: nothing is written.
    ///
    /// The payload is redacted ([`crate::redact::redact_json`]) before hashing
    /// and writing, so secrets never enter the hash chain or the file. When
    /// anything was redacted, object payloads gain a `redacted` field listing
    /// the kinds found; clean payloads are stored byte-identical.
    pub fn append(&mut self, kind: &str, payload: serde_json::Value) -> Result<Entry, LogError> {
        let guard = self.acquire_lock()?;
        self.append_locked(kind, payload, &guard)
    }

    /// Append one entry while holding `guard` from [`DecisionLog::acquire_lock`].
    ///
    /// Same write as [`DecisionLog::append`], but the caller supplies the
    /// lock: multi-step writers (e.g. `crate::approvals::release`) hold one
    /// guard across check-then-append so two processes cannot interleave.
    /// The guard must be for this same log; a mismatched guard, a lock
    /// failure, or a broken on-disk chain fails closed with no write.
    pub fn append_locked(
        &mut self,
        kind: &str,
        payload: serde_json::Value,
        guard: &LogLock,
    ) -> Result<Entry, LogError> {
        if guard.path != lock_path_for(&self.path) {
            return Err(LogError::Lock("guard is for a different log".to_string()));
        }
        let (mut payload, hits) = crate::redact::redact_json(&payload);
        if !hits.is_empty() {
            if let Some(obj) = payload.as_object_mut() {
                let kinds: Vec<serde_json::Value> = hits
                    .iter()
                    .map(|k| serde_json::Value::String(k.as_str().to_string()))
                    .collect();
                match obj.get_mut("redacted") {
                    Some(serde_json::Value::Array(existing)) => {
                        for k in kinds {
                            if !existing.contains(&k) {
                                existing.push(k);
                            }
                        }
                    }
                    _ => {
                        obj.insert("redacted".to_string(), serde_json::Value::Array(kinds));
                    }
                }
            }
        }
        // Re-read the on-disk head under the lock: another process may have
        // appended since this handle last wrote. A broken chain fails closed.
        match self.verify_or_empty()?.last() {
            Some(last) => {
                self.next_seq = last.seq + 1;
                self.last_hash = last.hash.clone();
            }
            None => {
                self.next_seq = 0;
                self.last_hash = GENESIS.into();
            }
        }
        let hash = digest(self.next_seq, &self.last_hash, kind, &payload);
        let e = Entry {
            seq: self.next_seq,
            prev_hash: self.last_hash.clone(),
            kind: kind.into(),
            payload,
            hash,
        };
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        writeln!(f, "{}", serde_json::to_string(&e)?)?;
        f.sync_data()?;
        self.next_seq += 1;
        self.last_hash = e.hash.clone();
        Ok(e)
    }

    /// Like `verify`, but a log file that does not exist yet is empty.
    pub fn verify_or_empty(&self) -> Result<Vec<Entry>, LogError> {
        if self.path.exists() {
            self.verify()
        } else {
            Ok(Vec::new())
        }
    }

    /// Read all entries and check the hash chain.
    pub fn verify(&self) -> Result<Vec<Entry>, LogError> {
        let mut prev = GENESIS.to_string();
        let mut out = Vec::new();
        for (i, line) in BufReader::new(File::open(&self.path)?).lines().enumerate() {
            let e: Entry = serde_json::from_str(&line?)?;
            if e.seq != i as u64
                || e.prev_hash != prev
                || e.hash != digest(e.seq, &e.prev_hash, &e.kind, &e.payload)
            {
                return Err(LogError::Chain(i));
            }
            prev = e.hash.clone();
            out.push(e);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn appends_and_reopens_preserving_history() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("d.jsonl");
        let mut log = DecisionLog::open(&p).unwrap();
        log.append("route", json!({"to": "local"})).unwrap();
        drop(log);
        let mut log = DecisionLog::open(&p).unwrap();
        let e = log.append("approve", json!({"id": 1})).unwrap();
        assert_eq!(e.seq, 1);
        assert_eq!(log.verify().unwrap().len(), 2);
    }

    #[test]
    fn detects_tampering() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("d.jsonl");
        let mut log = DecisionLog::open(&p).unwrap();
        log.append("a", json!(1)).unwrap();
        log.append("b", json!(2)).unwrap();
        let s = std::fs::read_to_string(&p)
            .unwrap()
            .replacen("\"a\"", "\"x\"", 1);
        std::fs::write(&p, s).unwrap();
        assert!(matches!(DecisionLog::open(&p), Err(LogError::Chain(0))));
    }

    #[test]
    fn append_redacts_secrets_before_hashing_and_writing() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("d.jsonl");
        let mut log = DecisionLog::open(&p).unwrap();
        // Scanner-safe fakes, built at runtime.
        let token = format!("ghp_{}", "C".repeat(36));
        let pass = "P".repeat(12);
        let e = log
            .append(
                "note",
                json!({
                    "summary": format!("rotate {token} now"),
                    "db": format!("postgres://syn_user:{pass}@example.invalid/db"),
                    "access_token": "Z".repeat(24),
                    "total_tokens": 150,
                }),
            )
            .unwrap();
        // The returned entry already carries the redacted payload.
        let mut kinds: Vec<&str> = e.payload["redacted"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        kinds.sort_unstable();
        assert_eq!(kinds, ["github_token", "json_key", "url_password"]);
        assert_eq!(e.payload["total_tokens"], 150);
        // Bytes on disk: no secret, tagged markers instead, chain verifies.
        let raw = std::fs::read_to_string(&p).unwrap();
        assert!(!raw.contains(&token), "token leaked: {raw}");
        assert!(!raw.contains(&pass), "password leaked: {raw}");
        assert!(!raw.contains(&"Z".repeat(24)), "key value leaked: {raw}");
        assert!(raw.contains("[redacted:github_token]"), "{raw}");
        assert!(raw.contains("[redacted:url_password]"), "{raw}");
        assert!(raw.contains("[redacted:json_key]"), "{raw}");
        let entries = log.verify().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0], e);
        // Reopening re-verifies the redacted chain.
        let log2 = DecisionLog::open(&p).unwrap();
        assert_eq!(log2.verify().unwrap().len(), 1);
    }

    #[test]
    fn load_verified_reports_first_broken_line() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("d.jsonl");
        let mut log = DecisionLog::open(&p).unwrap();
        log.append("a", json!(1)).unwrap();
        log.append("b", json!(2)).unwrap();
        // Valid file loads.
        assert_eq!(load_verified(&p).unwrap().len(), 2);
        // Same-length tamper on line 2 keeps JSON valid: hash must catch it.
        let raw = std::fs::read_to_string(&p)
            .unwrap()
            .replacen("\"b\"", "\"x\"", 1);
        std::fs::write(&p, raw).unwrap();
        match load_verified(&p).unwrap_err() {
            LoadError::Broken { line, reason } => {
                assert_eq!(line, 2);
                assert!(reason.contains("hash mismatch"), "{reason}");
            }
            e => panic!("want Broken, got {e}"),
        }
    }

    #[test]
    fn load_verified_rejects_truncated_last_line_empty_and_missing() {
        let dir = tempfile::tempdir().unwrap();
        // Truncated mid-line: the last line is invalid JSON.
        let p = dir.path().join("t.jsonl");
        let mut log = DecisionLog::open(&p).unwrap();
        log.append("a", json!({"v": 1})).unwrap();
        log.append("b", json!({"v": 2})).unwrap();
        let raw = std::fs::read_to_string(&p).unwrap();
        let cut = raw.len() - 5;
        std::fs::write(&p, &raw[..cut]).unwrap();
        match load_verified(&p).unwrap_err() {
            LoadError::Broken { line, reason } => {
                assert_eq!(line, 2);
                assert!(reason.contains("invalid JSON"), "{reason}");
            }
            e => panic!("want Broken, got {e}"),
        }
        // Empty file is a valid empty log.
        let e = dir.path().join("e.jsonl");
        std::fs::write(&e, "").unwrap();
        assert!(load_verified(&e).unwrap().is_empty());
        // Missing file is an error (fail closed), and stays missing.
        let m = dir.path().join("missing.jsonl");
        assert!(matches!(load_verified(&m).unwrap_err(), LoadError::Io(_)));
        assert!(!m.exists());
    }

    #[test]
    fn append_leaves_clean_payloads_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("d.jsonl");
        let mut log = DecisionLog::open(&p).unwrap();
        let payload = json!({
            "to": "local",
            "prompt_tokens": 100,
            "total_tokens": 150,
            "model": "mock-muse",
        });
        let e = log.append("route", payload.clone()).unwrap();
        assert_eq!(e.payload, payload);
        assert!(e.payload.get("redacted").is_none());
        let raw = std::fs::read_to_string(&p).unwrap();
        assert!(raw.contains("\"total_tokens\":150"), "{raw}");
        assert!(!raw.contains("redacted"), "{raw}");
    }

    /// Appending while the sibling lock path is unusable fails closed: the
    /// write returns a lock error and leaves no log file behind. Once the
    /// lock path is usable again, appends work and the chain verifies.
    #[test]
    fn append_fails_closed_when_lock_file_cannot_be_opened() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("d.jsonl");
        // A directory at the lock path makes the lock open fail (EISDIR).
        let lock_dir = dir.path().join("d.jsonl.lock");
        std::fs::create_dir(&lock_dir).unwrap();
        let mut log = DecisionLog::open(&p).unwrap();
        let err = log.append("a", json!(1)).unwrap_err();
        assert!(matches!(err, LogError::Lock(_)), "{err}");
        assert!(!p.exists(), "failed append must write nothing");
        // Recovery: with the lock path usable again, appends work and verify.
        std::fs::remove_dir(&lock_dir).unwrap();
        log.append("a", json!(1)).unwrap();
        assert_eq!(log.verify().unwrap().len(), 1);
    }
}
