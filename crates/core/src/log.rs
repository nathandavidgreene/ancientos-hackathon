//! Append-only, hash-chained JSONL decision log, with optional HMAC
//! signatures and a head-hash sidecar against tail truncation.
//!
//! Every entry links to the previous hash, so in-place edits break
//! [`DecisionLog::verify`]. Two gaps remain without the hardening below: whole
//! trailing lines can be deleted leaving a valid prefix chain, and anyone with
//! write access can forge a valid chain. This module closes both:
//! - Signed mode ([`DecisionLog::open_signed`]) attaches an HMAC-SHA256
//!   signature over each entry's chain hash, keyed by raw bytes from a
//!   mode-0600 key file. Verification fails closed on a missing key, a
//!   missing signature, or a bad signature.
//! - Every append (signed or not) records the new head (entry count plus head
//!   hash) in a `<log>.head` sidecar under the existing exclusive lock, and
//!   verification fails when the log is shorter than or diverges from it.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
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
    /// The signing key could not be loaded (missing file, bad mode, too
    /// short) or is required but absent: fail closed, nothing was written and
    /// nothing verified. Never carries key material, only the key path.
    #[error("key: {0}")]
    Key(String),
    /// Signed-mode verification found entry `0` (0-based) without a
    /// signature: unsigned entries never verify when a key is loaded.
    #[error("entry {0} is unsigned")]
    Unsigned(usize),
    /// Signed-mode verification found entry `0` (0-based) with a signature
    /// that does not match its chain hash under the loaded key.
    #[error("entry {0} has a bad signature")]
    BadSignature(usize),
    /// The head sidecar records `recorded` entries but the log holds `found`:
    /// trailing lines were deleted. Fail closed: the prefix may look valid.
    #[error("truncated: head sidecar records {recorded} entries but log has {found}")]
    Truncated { recorded: u64, found: usize },
    /// The head sidecar's recorded head hash does not match the log at that
    /// position, or the sidecar itself is malformed: fail closed.
    #[error("head mismatch: {0}")]
    HeadMismatch(String),
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
/// writes, or truncates the file: read-only opens of the log and its head
/// sidecar only. Fail closed: an unreadable line, invalid JSON, a seq gap, a
/// `prev_hash` mismatch, a hash mismatch, a deleted tail (fewer entries than
/// the sidecar records), or a diverged head is an error naming that line.
/// Entry signatures are not checked here (no key is loaded); use
/// [`DecisionLog::open_signed`] plus [`DecisionLog::verify`] to verify them.
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
    // Chain first: an edited line reports its own line before the head check.
    if let Some(d) =
        head_discrepancy(path, &out).map_err(|e| broken(out.len() + 1, e.to_string()))?
    {
        return Err(match d {
            HeadDiscrepancy::Truncated { recorded, found } => broken(
                found + 1,
                format!("truncated: sidecar records {recorded} entries but log has {found}"),
            ),
            HeadDiscrepancy::Mismatch { recorded, line } => broken(
                line,
                format!("head mismatch: sidecar records {recorded} at line {line}"),
            ),
        });
    }
    Ok(out)
}

/// One hash-chained log entry: sequence number, previous hash, kind,
/// redacted payload, chain hash, and (signed mode only) an HMAC signature.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Entry {
    /// Zero-based position in the chain; must equal the line index.
    pub seq: u64,
    /// Chain hash of the previous entry, or `"0"` for the first.
    pub prev_hash: String,
    /// Entry kind (for example `route`, `approval.requested`).
    pub kind: String,
    /// Redacted payload; secrets never reach the hash or the file.
    pub payload: serde_json::Value,
    /// Chain digest over seq, previous hash, kind and payload.
    pub hash: String,
    /// Hex HMAC-SHA256 of [`Self::hash`] under the log signing key, or
    /// `None` for entries written without a key. Old lines without the field
    /// parse as `None`; unsigned appends omit it, staying byte-identical.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sig: Option<String>,
}

const GENESIS: &str = "0";

/// Hash-chain digest of a log entry (seq, previous hash, kind, payload).
fn digest(seq: u64, prev: &str, kind: &str, payload: &serde_json::Value) -> String {
    let mut h = Sha256::new();
    h.update(format!("{seq}|{prev}|{kind}|{payload}"));
    hex::encode(h.finalize())
}

/// Env var holding the log signing-key file path, read by
/// [`DecisionLog::open_with_key_env`] through an injected reader so tests
/// never touch the real environment. Unset or empty means legacy unsigned
/// mode; any other value must load via [`load_signing_key`], else open fails.
pub const LOG_KEY_ENV_VAR: &str = "ANCIENTOS_LOG_KEY_PATH";

/// Shortest accepted signing key in raw bytes (128 bits). Shorter files fail
/// closed; use 32 or more random bytes (for example from `getrandom`).
pub const MIN_KEY_LEN: usize = 16;

/// HMAC signing key bytes. `Debug` redacts the material: keys are never
/// printed, and [`LogError::Key`] carries the key path only.
#[derive(Clone)]
struct SigningKey(Vec<u8>);

impl std::fmt::Debug for SigningKey {
    /// Debug output with the key bytes redacted.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("SigningKey").field(&"[redacted]").finish()
    }
}

/// HMAC-SHA256 of `msg` under `key`, per RFC 2104 (block size 64, keys
/// longer than a block hashed first). Pure computation over the existing
/// `sha2` dependency; never fails.
fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut padded = [0u8; BLOCK];
    if key.len() > BLOCK {
        padded[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        padded[..key.len()].copy_from_slice(key);
    }
    let mut ipad = [0x36u8; BLOCK];
    let mut opad = [0x5cu8; BLOCK];
    for i in 0..BLOCK {
        ipad[i] ^= padded[i];
        opad[i] ^= padded[i];
    }
    let mut inner = Sha256::new();
    inner.update(ipad);
    inner.update(msg);
    let mut outer = Sha256::new();
    outer.update(opad);
    outer.update(inner.finalize());
    outer.finalize().into()
}

/// Expected hex signature for chain hash `hash` under `key`.
fn expected_sig(key: &SigningKey, hash: &str) -> String {
    hex::encode(hmac_sha256(&key.0, hash.as_bytes()))
}

/// Constant-time hex-string equality for signature checks: every byte pair
/// is compared, so a mismatch position changes nothing about the timing. A
/// length difference returns false at once (lengths are not secret: every
/// valid signature is 64 hex chars).
fn sig_equal(a: &str, b: &str) -> bool {
    let (x, y) = (a.as_bytes(), b.as_bytes());
    if x.len() != y.len() {
        return false;
    }
    x.iter()
        .zip(y.iter())
        .fold(0u8, |acc, (p, q)| acc | (p ^ q))
        == 0
}

/// Load raw HMAC key bytes from `path`, failing closed: a missing or
/// unreadable file, a mode other than exactly 0600 (owner read/write only,
/// checked via the unix permission bits), or fewer than [`MIN_KEY_LEN`]
/// bytes is a [`LogError::Key`]. The bytes are used verbatim (no trimming);
/// the error names the path but never the material.
pub fn load_signing_key(path: &Path) -> Result<Vec<u8>, LogError> {
    let mode = std::fs::metadata(path)
        .map_err(|e| LogError::Key(format!("cannot stat {}: {e}", path.display())))?
        .permissions()
        .mode()
        & 0o777;
    if mode != 0o600 {
        return Err(LogError::Key(format!(
            "refusing {}: mode is {mode:04o}, want 0600",
            path.display()
        )));
    }
    let key = std::fs::read(path)
        .map_err(|e| LogError::Key(format!("cannot read {}: {e}", path.display())))?;
    if key.len() < MIN_KEY_LEN {
        return Err(LogError::Key(format!(
            "refusing {}: {} bytes, want at least {MIN_KEY_LEN}",
            path.display(),
            key.len()
        )));
    }
    Ok(key)
}

/// Persisted head record in the `<log>.head` sidecar: how many entries the
/// log held after the last append, and the head (last) chain hash then.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct HeadRecord {
    /// Entry count after the append that wrote this record.
    count: u64,
    /// Chain hash of the last entry at that count.
    head: String,
}

/// Sibling head-hash file for a log path: `<path>.head`, written on every
/// append under the existing exclusive log lock.
fn head_path_for(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".head");
    PathBuf::from(s)
}

/// Sibling temp file used to update the head sidecar atomically.
fn head_tmp_path_for(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".head.tmp");
    PathBuf::from(s)
}

/// Read the head sidecar for `log_path`: `Ok(None)` when no sidecar exists
/// yet (a log that predates sidecars, or nothing appended so far). Any other
/// read failure or invalid JSON propagates; a well-formed record whose head
/// is not a 64-hex hash with a nonzero count is a [`LogError::HeadMismatch`].
fn read_head_record(log_path: &Path) -> Result<Option<HeadRecord>, LogError> {
    let sidecar = head_path_for(log_path);
    let text = match std::fs::read_to_string(&sidecar) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(LogError::HeadMismatch(format!(
                "cannot read {}: {e}",
                sidecar.display()
            )))
        }
        Ok(t) => t,
    };
    let rec: HeadRecord = serde_json::from_str(&text)?;
    let hex_ok = rec.head.len() == 64 && rec.head.bytes().all(|b| b.is_ascii_hexdigit());
    if rec.count > 0 && !hex_ok {
        return Err(LogError::HeadMismatch(format!(
            "malformed record in {}",
            sidecar.display()
        )));
    }
    Ok(Some(rec))
}

/// Persist `count` entries ending in `head` to the sidecar, atomically
/// (temp file plus fsync plus rename). The caller must hold the log's
/// exclusive lock; a failure returns the I/O error and the sidecar stays at
/// its previous (stale but prefix-consistent) record.
fn write_head_record(log_path: &Path, count: u64, head: &str) -> Result<(), LogError> {
    let rec = HeadRecord {
        count,
        head: head.to_string(),
    };
    let tmp = head_tmp_path_for(log_path);
    let mut f = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&tmp)?;
    writeln!(f, "{}", serde_json::to_string(&rec)?)?;
    f.sync_data()?;
    drop(f);
    std::fs::rename(&tmp, head_path_for(log_path))?;
    Ok(())
}

/// How a verified entry list disagrees with its head sidecar, if at all.
#[derive(Debug, Clone, PartialEq)]
enum HeadDiscrepancy {
    /// The sidecar records more entries than the log holds: tail deleted.
    Truncated { recorded: u64, found: usize },
    /// The entry at 1-based `line` (`recorded` count) does not carry the
    /// recorded head hash.
    Mismatch { recorded: String, line: usize },
}

/// Compare verified `entries` for the log at `log_path` against its head
/// sidecar: `Ok(None)` when there is no sidecar yet, when the sidecar is
/// stale-behind a longer log with a matching prefix (a crash between the log
/// write and the sidecar write), or when head and count agree. A shorter log
/// or a diverged prefix is `Some(_)`. Read failures and malformed sidecars
/// are `Err(_)` (fail closed). Pure read: writes nothing.
fn head_discrepancy(
    log_path: &Path,
    entries: &[Entry],
) -> Result<Option<HeadDiscrepancy>, LogError> {
    let rec = match read_head_record(log_path) {
        Ok(None) => return Ok(None),
        Ok(Some(r)) => r,
        Err(e) => return Err(e),
    };
    if rec.count == 0 {
        return Ok(None);
    }
    let recorded = usize::try_from(rec.count).unwrap_or(usize::MAX);
    if entries.len() < recorded {
        return Ok(Some(HeadDiscrepancy::Truncated {
            recorded: rec.count,
            found: entries.len(),
        }));
    }
    if entries[recorded - 1].hash != rec.head {
        return Ok(Some(HeadDiscrepancy::Mismatch {
            recorded: rec.head,
            line: recorded,
        }));
    }
    Ok(None)
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
///
/// Unsigned mode ([`DecisionLog::open`]) verifies the hash chain and the
/// head sidecar only, ignoring any signature fields; signed mode
/// ([`DecisionLog::open_signed`]) additionally signs every append with
/// HMAC-SHA256 and rejects unsigned or bad signatures on verify. Callers
/// that require authenticity must use signed mode: legacy `open` cannot tell
/// a forged-but-valid chain from a signed one. The key bytes stay redacted
/// from `Debug`.
#[derive(Debug)]
pub struct DecisionLog {
    path: PathBuf,
    next_seq: u64,
    last_hash: String,
    /// Signing key, or `None` for legacy unsigned mode.
    key: Option<SigningKey>,
}

impl DecisionLog {
    /// Open (or create) a log, verifying the existing chain and head.
    ///
    /// Legacy unsigned mode: appends carry no signature (byte-identical to
    /// before) and [`DecisionLog::verify`] checks the chain plus the head
    /// sidecar only. A missing file opens as empty; an existing file with a
    /// broken chain, a truncated tail or a diverged head fails closed.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, LogError> {
        Self::open_with_key(path, None)
    }

    /// Open (or create) a log in signed mode, loading the HMAC key from
    /// `key_path` via [`load_signing_key`].
    ///
    /// Fails closed before any I/O to the log when the key is missing,
    /// unreadable, not mode 0600, or shorter than [`MIN_KEY_LEN`]: no handle
    /// means no writes and no verified reads. The existing file (if any)
    /// must then verify fully — chain, every signature, and head — so
    /// enabling signing on a log with pre-existing unsigned entries fails
    /// with [`LogError::Unsigned`]: start a fresh log instead.
    pub fn open_signed(
        path: impl AsRef<Path>,
        key_path: impl AsRef<Path>,
    ) -> Result<Self, LogError> {
        let key = load_signing_key(key_path.as_ref())?;
        Self::open_with_key(path, Some(SigningKey(key)))
    }

    /// Open a log, resolving the signing-key path from `env`
    /// (`[`LOG_KEY_ENV_VAR`]`), so tests inject fakes and never touch the
    /// real environment. Unset or empty means [`DecisionLog::open`]
    /// (unsigned); any other value means [`DecisionLog::open_signed`] with
    /// that path, failing closed on any key problem.
    pub fn open_with_key_env(
        path: impl AsRef<Path>,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Self, LogError> {
        match env(LOG_KEY_ENV_VAR).filter(|v| !v.is_empty()) {
            Some(p) => Self::open_signed(path, p),
            None => {
                eprintln!("WARNING: ANCIENTOS_LOG_KEY_PATH is unset; decision log is UNSIGNED and tail truncation protection is not authenticated");
                Self::open(path)
            }
        }
    }

    /// Open with an already-loaded key (`None` for unsigned mode), verifying
    /// the existing chain (plus signatures and head) before returning.
    fn open_with_key(path: impl AsRef<Path>, key: Option<SigningKey>) -> Result<Self, LogError> {
        let path = path.as_ref().to_path_buf();
        let mut log = Self {
            path,
            next_seq: 0,
            last_hash: GENESIS.into(),
            key,
        };
        if log.path.exists() {
            for e in log.verify()? {
                log.next_seq = e.seq + 1;
                log.last_hash = e.hash;
            }
        }
        Ok(log)
    }

    /// True when this handle signs appends and verifies signatures.
    pub fn is_signed(&self) -> bool {
        self.key.is_some()
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
    /// Signed handles attach an HMAC signature, and every append records the
    /// new head in the `<log>.head` sidecar under the same lock (see
    /// [`DecisionLog::append_locked`]).
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
    ///
    /// Signed handles attach an HMAC-SHA256 signature over the new chain
    /// hash; unsigned handles write no signature field. The new head (count
    /// plus hash) is then recorded in the `<log>.head` sidecar under this
    /// same lock, so a later tail truncation is detectable. A sidecar write
    /// failure fails the append: the entry is already on disk, the stale
    /// sidecar stays prefix-consistent, and the next successful append
    /// repairs it.
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
        let sig = self.key.as_ref().map(|k| expected_sig(k, &hash));
        let e = Entry {
            seq: self.next_seq,
            prev_hash: self.last_hash.clone(),
            kind: kind.into(),
            payload,
            hash,
            sig,
        };
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        writeln!(f, "{}", serde_json::to_string(&e)?)?;
        f.sync_data()?;
        self.next_seq += 1;
        self.last_hash = e.hash.clone();
        write_head_record(&self.path, self.next_seq, &self.last_hash)?;
        Ok(e)
    }

    /// Like `verify`, but a log file that does not exist yet reads as empty
    /// — unless its head sidecar records entries, in which case the whole
    /// log was deleted and [`LogError::Truncated`] fails closed instead of
    /// reporting a misleading empty history.
    pub fn verify_or_empty(&self) -> Result<Vec<Entry>, LogError> {
        if self.path.exists() {
            self.verify()
        } else if let Some(rec) = read_head_record(&self.path)? {
            if rec.count > 0 {
                return Err(LogError::Truncated {
                    recorded: rec.count,
                    found: 0,
                });
            }
            Ok(Vec::new())
        } else {
            Ok(Vec::new())
        }
    }

    /// Read all entries and check the hash chain, then signatures (signed
    /// mode only), then the head sidecar.
    ///
    /// Each entry must continue the sequence, link `prev_hash`, and match its
    /// recomputed chain hash, else [`LogError::Chain`]. On signed handles
    /// each entry must also carry a valid HMAC signature, else
    /// [`LogError::Unsigned`] or [`LogError::BadSignature`] (constant-time
    /// comparison); unsigned handles ignore signature fields. Finally the
    /// entry list is compared against the `<log>.head` sidecar: a shorter log
    /// is [`LogError::Truncated`] and a diverged head is
    /// [`LogError::HeadMismatch`]. A missing sidecar (pre-sidecar logs)
    /// skips that check; a malformed one fails closed.
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
            if let Some(key) = &self.key {
                match &e.sig {
                    None => return Err(LogError::Unsigned(i)),
                    Some(s) if !sig_equal(s, &expected_sig(key, &e.hash)) => {
                        return Err(LogError::BadSignature(i))
                    }
                    Some(_) => {}
                }
            }
            prev = e.hash.clone();
            out.push(e);
        }
        // Chain (and signatures) first: an edited line reports its own index
        // before the head check runs.
        if let Some(d) = head_discrepancy(&self.path, &out)? {
            return Err(match d {
                HeadDiscrepancy::Truncated { recorded, found } => {
                    LogError::Truncated { recorded, found }
                }
                HeadDiscrepancy::Mismatch { recorded, line } => {
                    LogError::HeadMismatch(format!("sidecar records {recorded} at line {line}"))
                }
            });
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

    /// Test helper: write `bytes` to `path` with mode 0600 (fresh files
    /// inherit the umask, so the mode is set explicitly).
    fn write_key_0600(path: &Path, bytes: &[u8]) {
        std::fs::write(path, bytes).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    /// Test helper: 32 synthetic key bytes (never a real secret).
    fn test_key() -> Vec<u8> {
        (0u8..32).collect()
    }

    /// Test helper: 32 synthetic key bytes distinct from [`test_key`].
    fn other_key() -> Vec<u8> {
        (100u8..132).collect()
    }

    /// Test helper: sidecar path for a log path (`<log>.head`).
    fn head_sidecar_for(log: &Path) -> PathBuf {
        let mut s = log.as_os_str().to_owned();
        s.push(".head");
        PathBuf::from(s)
    }

    /// Test helper: open `name` under `dir` with `env`, assert unsigned
    /// mode, append once, and assert no signature field lands on disk.
    fn assert_unsigned_mode(dir: &Path, name: &str, env: &dyn Fn(&str) -> Option<String>) {
        let q = dir.join(name);
        let mut log = DecisionLog::open_with_key_env(&q, env).unwrap();
        assert!(!log.is_signed());
        log.append("a", json!(1)).unwrap();
        assert!(!std::fs::read_to_string(&q).unwrap().contains("\"sig\""));
    }

    /// HMAC-SHA256 matches RFC 4231 test case 1 (20x 0x0b key, "Hi There").
    #[test]
    fn hmac_sha256_matches_rfc4231_vector() {
        assert_eq!(
            hex::encode(hmac_sha256(&[0x0bu8; 20], b"Hi There")),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
    }

    /// Signed appends carry 64-hex HMACs over their chain hash and verify
    /// with the same key; opening with a different key fails closed at the
    /// first entry, while legacy unsigned opens still verify the chain.
    #[test]
    fn signed_appends_verify_with_same_key_and_reject_other_key() {
        let dir = tempfile::tempdir().unwrap();
        let key_a = dir.path().join("a.key");
        let key_b = dir.path().join("b.key");
        write_key_0600(&key_a, &test_key());
        write_key_0600(&key_b, &other_key());
        let p = dir.path().join("d.jsonl");
        let mut log = DecisionLog::open_signed(&p, &key_a).unwrap();
        assert!(log.is_signed());
        let a = log.append("a", json!(1)).unwrap();
        let b = log.append("b", json!(2)).unwrap();
        for e in [&a, &b] {
            let s = e.sig.as_ref().expect("signed append carries sig");
            assert_eq!(s.len(), 64, "{s}");
            assert!(s.bytes().all(|c| c.is_ascii_hexdigit()), "{s}");
            assert_eq!(*s, hex::encode(hmac_sha256(&test_key(), e.hash.as_bytes())));
        }
        assert_eq!(log.verify().unwrap().len(), 2);
        drop(log);
        assert!(matches!(
            DecisionLog::open_signed(&p, &key_b).unwrap_err(),
            LogError::BadSignature(0)
        ));
        // Legacy unsigned handles verify the chain only, sigs ignored.
        let legacy = DecisionLog::open(&p).unwrap();
        assert!(!legacy.is_signed());
        assert_eq!(legacy.verify().unwrap().len(), 2);
    }

    /// Flipping one signature byte on disk keeps the chain parseable but
    /// fails signed verification with the entry index.
    #[test]
    fn flipped_signature_byte_fails_signed_verify() {
        let dir = tempfile::tempdir().unwrap();
        let key_path = dir.path().join("log.key");
        write_key_0600(&key_path, &test_key());
        let p = dir.path().join("d.jsonl");
        let mut log = DecisionLog::open_signed(&p, &key_path).unwrap();
        log.append("a", json!(1)).unwrap();
        log.append("b", json!(2)).unwrap();
        drop(log);
        let raw = std::fs::read_to_string(&p).unwrap();
        let mut lines: Vec<String> = raw.lines().map(str::to_string).collect();
        let mut last: serde_json::Value = serde_json::from_str(&lines[1]).unwrap();
        let sig = last["sig"].as_str().unwrap().to_string();
        let mut bytes = sig.into_bytes();
        bytes[0] = if bytes[0] == b'a' { b'b' } else { b'a' };
        last["sig"] = serde_json::Value::String(String::from_utf8(bytes).unwrap());
        lines[1] = serde_json::to_string(&last).unwrap();
        std::fs::write(&p, lines.join("\n") + "\n").unwrap();
        assert!(matches!(
            DecisionLog::open_signed(&p, &key_path).unwrap_err(),
            LogError::BadSignature(1)
        ));
    }

    /// Deleting the signature field keeps a valid chain but signed
    /// verification reports the entry as unsigned.
    #[test]
    fn stripped_signature_reports_unsigned() {
        let dir = tempfile::tempdir().unwrap();
        let key_path = dir.path().join("log.key");
        write_key_0600(&key_path, &test_key());
        let p = dir.path().join("d.jsonl");
        let mut log = DecisionLog::open_signed(&p, &key_path).unwrap();
        log.append("a", json!(1)).unwrap();
        log.append("b", json!(2)).unwrap();
        drop(log);
        let raw = std::fs::read_to_string(&p).unwrap();
        let mut lines: Vec<String> = raw.lines().map(str::to_string).collect();
        let mut last: serde_json::Value = serde_json::from_str(&lines[1]).unwrap();
        last.as_object_mut().unwrap().remove("sig");
        lines[1] = serde_json::to_string(&last).unwrap();
        std::fs::write(&p, lines.join("\n") + "\n").unwrap();
        assert!(matches!(
            DecisionLog::open_signed(&p, &key_path).unwrap_err(),
            LogError::Unsigned(1)
        ));
    }

    /// A missing, short, or group/other-readable key fails signed open with
    /// [`LogError::Key`] and creates neither the log nor its sidecar.
    #[test]
    fn open_signed_fails_closed_on_missing_short_or_exposed_key() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("d.jsonl");
        let err = DecisionLog::open_signed(&p, dir.path().join("missing.key")).unwrap_err();
        assert!(matches!(err, LogError::Key(_)), "{err}");
        assert!(!p.exists());
        assert!(!head_sidecar_for(&p).exists());
        let short = dir.path().join("short.key");
        write_key_0600(&short, b"tiny");
        let err = DecisionLog::open_signed(&p, &short).unwrap_err();
        assert!(matches!(err, LogError::Key(_)), "{err}");
        let exposed = dir.path().join("exposed.key");
        std::fs::write(&exposed, test_key()).unwrap();
        std::fs::set_permissions(&exposed, std::fs::Permissions::from_mode(0o644)).unwrap();
        let err = DecisionLog::open_signed(&p, &exposed).unwrap_err();
        assert!(matches!(err, LogError::Key(_)), "{err}");
        assert!(err.to_string().contains("0600"), "{err}");
        assert!(!p.exists());
        assert!(!head_sidecar_for(&p).exists());
    }

    /// Key bytes never leak: the log's `Debug` redacts them and key errors
    /// carry the path only.
    #[test]
    fn key_material_never_leaks_into_debug_or_error_text() {
        let marker = b"SYNTHETIC-KEY-MATERIAL-0123456789".to_vec();
        assert!(marker.len() >= MIN_KEY_LEN);
        let dir = tempfile::tempdir().unwrap();
        let key_path = dir.path().join("log.key");
        write_key_0600(&key_path, &marker);
        let p = dir.path().join("d.jsonl");
        let mut log = DecisionLog::open_signed(&p, &key_path).unwrap();
        log.append("a", json!(1)).unwrap();
        let debug = format!("{log:?}");
        assert!(!debug.contains("SYNTHETIC"), "{debug}");
        assert!(!debug.contains(&hex::encode(&marker)), "{debug}");
        drop(log);
        std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let err = DecisionLog::open_signed(&p, &key_path)
            .unwrap_err()
            .to_string();
        assert!(!err.contains("SYNTHETIC"), "{err}");
        assert!(!err.contains(&hex::encode(&marker)), "{err}");
    }

    /// Every append records count plus head hash in the sidecar; deleting a
    /// whole trailing line then fails verification as truncated in both
    /// unsigned and signed modes (and in `load_verified`).
    #[test]
    fn tail_truncation_fails_verify_in_both_modes() {
        let dir = tempfile::tempdir().unwrap();
        let key_path = dir.path().join("log.key");
        write_key_0600(&key_path, &test_key());
        for (name, signed) in [("u.jsonl", false), ("s.jsonl", true)] {
            let p = dir.path().join(name);
            let mut log = if signed {
                DecisionLog::open_signed(&p, &key_path).unwrap()
            } else {
                DecisionLog::open(&p).unwrap()
            };
            log.append("a", json!(1)).unwrap();
            log.append("b", json!(2)).unwrap();
            let head = log.append("c", json!(3)).unwrap().hash.clone();
            drop(log);
            let sidecar: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(head_sidecar_for(&p)).unwrap())
                    .unwrap();
            assert_eq!(sidecar["count"], 3);
            assert_eq!(sidecar["head"], head);
            let raw = std::fs::read_to_string(&p).unwrap();
            let kept: Vec<&str> = raw.lines().take(2).collect();
            std::fs::write(&p, kept.join("\n") + "\n").unwrap();
            let err = if signed {
                DecisionLog::open_signed(&p, &key_path).unwrap_err()
            } else {
                DecisionLog::open(&p).unwrap_err()
            };
            assert!(
                matches!(
                    err,
                    LogError::Truncated {
                        recorded: 3,
                        found: 2
                    }
                ),
                "{name}: {err}"
            );
            match load_verified(&p).unwrap_err() {
                LoadError::Broken { line, reason } => {
                    assert_eq!(line, 3, "{name}");
                    assert!(reason.contains("truncated"), "{name}: {reason}");
                }
                e => panic!("{name}: want Broken, got {e}"),
            }
        }
    }

    /// A sidecar whose head diverges from the log, or that is not JSON,
    /// fails verification instead of verifying a rewritten history.
    #[test]
    fn diverged_or_malformed_head_sidecar_fails_verify() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("d.jsonl");
        let mut log = DecisionLog::open(&p).unwrap();
        log.append("a", json!(1)).unwrap();
        log.append("b", json!(2)).unwrap();
        drop(log);
        let sidecar = head_sidecar_for(&p);
        std::fs::write(&sidecar, r#"{"count":2,"head":"00"}"#).unwrap();
        assert!(matches!(
            DecisionLog::open(&p).unwrap_err(),
            LogError::HeadMismatch(_)
        ));
        std::fs::write(
            &sidecar,
            format!(r#"{{"count":2,"head":"{}"}}"#, "0".repeat(64)),
        )
        .unwrap();
        assert!(matches!(
            DecisionLog::open(&p).unwrap_err(),
            LogError::HeadMismatch(_)
        ));
        std::fs::write(&sidecar, "{ not json\n").unwrap();
        assert!(matches!(
            DecisionLog::open(&p).unwrap_err(),
            LogError::Json(_)
        ));
    }

    /// [`DecisionLog::open_with_key_env`] signs only when the key-path
    /// variable is set and non-empty; unsigned appends stay byte-identical
    /// (no signature field on disk).
    #[test]
    fn open_with_key_env_selects_signed_mode_only_when_set() {
        let dir = tempfile::tempdir().unwrap();
        let key_path = dir.path().join("log.key");
        write_key_0600(&key_path, &test_key());
        let key_str = key_path.to_string_lossy().into_owned();
        let env_some = |k: &str| (k == LOG_KEY_ENV_VAR).then(|| key_str.clone());
        let p = dir.path().join("s.jsonl");
        let mut log = DecisionLog::open_with_key_env(&p, &env_some).unwrap();
        assert!(log.is_signed());
        log.append("a", json!(1)).unwrap();
        assert!(std::fs::read_to_string(&p).unwrap().contains("\"sig\""));
        let env_none = |_: &str| None::<String>;
        let env_empty = |k: &str| (k == LOG_KEY_ENV_VAR).then(String::new);
        assert_unsigned_mode(dir.path(), "u0.jsonl", &env_none);
        assert_unsigned_mode(dir.path(), "u1.jsonl", &env_empty);
    }

    /// Appending onto a log truncated after the handle opened fails closed:
    /// the re-read under the lock sees the truncation and writes nothing.
    #[test]
    fn append_after_truncation_fails_closed_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("d.jsonl");
        let mut log = DecisionLog::open(&p).unwrap();
        log.append("a", json!(1)).unwrap();
        log.append("b", json!(2)).unwrap();
        let raw = std::fs::read_to_string(&p).unwrap();
        let first = raw.lines().next().unwrap().to_string();
        std::fs::write(&p, first.clone() + "\n").unwrap();
        let err = log.append("c", json!(3)).unwrap_err();
        assert!(
            matches!(
                err,
                LogError::Truncated {
                    recorded: 2,
                    found: 1
                }
            ),
            "{err}"
        );
        assert_eq!(std::fs::read_to_string(&p).unwrap(), first + "\n");
    }

    /// Deleting the whole log file reads as truncated (not empty) while its
    /// sidecar survives; a truly fresh path still reads as empty.
    #[test]
    fn deleted_log_with_sidecar_reads_as_truncated_not_empty() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("d.jsonl");
        let mut log = DecisionLog::open(&p).unwrap();
        log.append("a", json!(1)).unwrap();
        drop(log);
        std::fs::remove_file(&p).unwrap();
        let log = DecisionLog::open(&p).unwrap();
        assert!(matches!(
            log.verify_or_empty().unwrap_err(),
            LogError::Truncated {
                recorded: 1,
                found: 0
            }
        ));
        let fresh = DecisionLog::open(dir.path().join("fresh.jsonl")).unwrap();
        assert!(fresh.verify_or_empty().unwrap().is_empty());
    }
}
