//! Token storage: OAuth tokens as JSON with strict file permissions.
//!
//! Location is `$ANCIENTOS_HOME/x_tokens.json`, defaulting to
//! `~/.ancientos-hackathon/x_tokens.json`. The directory is created with mode
//! 0700 and the file is written atomically with mode 0600. Tokens are never
//! printed: `Debug` redacts them.
use serde::{Deserialize, Serialize};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

pub const TOKENS_FILE_NAME: &str = "x_tokens.json";
pub const HOME_DIR_NAME: &str = ".ancientos-hackathon";

/// Seconds of clock skew tolerated when checking expiry.
pub const EXPIRY_SKEW_SECS: u64 = 30;

#[derive(Debug, thiserror::Error)]
pub enum TokenStoreError {
    #[error("no home: neither ANCIENTOS_HOME nor HOME is set")]
    NoHome,
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("parse: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("clock error: {0}")]
    Clock(String),
}

/// Stored OAuth tokens. `Debug` redacts both token values.
#[derive(Clone, Serialize, Deserialize)]
pub struct StoredTokens {
    pub access_token: String,
    pub refresh_token: String,
    /// Access-token expiry as unix seconds.
    pub expires_at: u64,
    pub scope: String,
}

impl std::fmt::Debug for StoredTokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoredTokens")
            .field("access_token", &"[redacted]")
            .field("refresh_token", &"[redacted]")
            .field("expires_at", &self.expires_at)
            .field("scope", &self.scope)
            .finish()
    }
}

impl StoredTokens {
    pub fn new(
        access_token: String,
        refresh_token: String,
        expires_at: u64,
        scope: String,
    ) -> Self {
        Self {
            access_token,
            refresh_token,
            expires_at,
            scope,
        }
    }

    /// True when `now` (unix secs) is past expiry, allowing skew.
    pub fn is_expired_at(&self, now: u64) -> bool {
        now.saturating_add(EXPIRY_SKEW_SECS) >= self.expires_at
    }

    /// True when the access token is expired. Fails closed (expired) when the
    /// clock cannot be read.
    pub fn is_expired(&self) -> bool {
        unix_now().map(|n| self.is_expired_at(n)).unwrap_or(true)
    }
}

pub fn unix_now() -> Result<u64, TokenStoreError> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .map_err(|e| TokenStoreError::Clock(e.to_string()))
}

/// Data directory: `$ANCIENTOS_HOME` or `~/.ancientos-hackathon`.
pub fn data_dir() -> Result<PathBuf, TokenStoreError> {
    if let Ok(dir) = std::env::var("ANCIENTOS_HOME") {
        if !dir.is_empty() {
            return Ok(PathBuf::from(dir));
        }
    }
    let home = std::env::var("HOME").map_err(|_| TokenStoreError::NoHome)?;
    if home.is_empty() {
        return Err(TokenStoreError::NoHome);
    }
    Ok(PathBuf::from(home).join(HOME_DIR_NAME))
}

/// Full path of the tokens file.
pub fn token_path() -> Result<PathBuf, TokenStoreError> {
    Ok(data_dir()?.join(TOKENS_FILE_NAME))
}

pub(crate) fn ensure_dir_0700(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

/// Write `data` to `path` atomically (temp file + rename) with mode 0600.
pub(crate) fn atomic_write_0600(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let tmp_name = format!(
        ".{}.tmp.{}",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("tokens"),
        std::process::id()
    );
    let tmp = path.with_file_name(tmp_name);
    {
        #[cfg(unix)]
        let file = {
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)?
        };
        #[cfg(not(unix))]
        let file = std::fs::File::create(&tmp)?;
        use std::io::Write;
        let mut file = file;
        file.write_all(data)?;
        file.sync_all()?;
    }
    #[cfg(unix)]
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Save tokens to an explicit path (parents created with mode 0700).
pub fn save_to(path: &Path, tokens: &StoredTokens) -> Result<(), TokenStoreError> {
    if let Some(parent) = path.parent() {
        ensure_dir_0700(parent)?;
    }
    let data = serde_json::to_string_pretty(tokens)?;
    atomic_write_0600(path, data.as_bytes())?;
    Ok(())
}

/// Save tokens to the default path. Returns the path written.
pub fn save(tokens: &StoredTokens) -> Result<PathBuf, TokenStoreError> {
    let path = token_path()?;
    save_to(&path, tokens)?;
    Ok(path)
}

/// Load tokens from an explicit path.
pub fn load_from(path: &Path) -> Result<StoredTokens, TokenStoreError> {
    Ok(serde_json::from_str(&std::fs::read_to_string(path)?)?)
}

/// Load tokens from the default path.
pub fn load() -> Result<StoredTokens, TokenStoreError> {
    load_from(&token_path()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard};

    fn env_lock() -> MutexGuard<'static, ()> {
        static ENV_MUTEX: Mutex<()> = Mutex::new(());
        ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn sample() -> StoredTokens {
        StoredTokens::new(
            "access-123".into(),
            "refresh-456".into(),
            1_800_000_000,
            "tweet.read users.read bookmark.read offline.access".into(),
        )
    }

    #[test]
    fn roundtrip_keeps_tokens() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join(TOKENS_FILE_NAME);
        save_to(&path, &sample()).unwrap();
        let back = load_from(&path).unwrap();
        assert_eq!(back.access_token, "access-123");
        assert_eq!(back.refresh_token, "refresh-456");
        assert_eq!(back.expires_at, 1_800_000_000);
        assert!(back.scope.contains("bookmark.read"));
    }

    #[test]
    fn debug_redacts_token_values() {
        let dbg = format!("{:?}", sample());
        assert!(!dbg.contains("access-123"), "leaked: {dbg}");
        assert!(!dbg.contains("refresh-456"), "leaked: {dbg}");
        assert!(dbg.contains("[redacted]"));
        assert!(dbg.contains("1800000000"));
    }

    #[cfg(unix)]
    #[test]
    fn file_is_0600_and_dir_is_0700() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join(TOKENS_FILE_NAME);
        save_to(&path, &sample()).unwrap();
        let file_mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(file_mode, 0o600, "file mode is {file_mode:o}");
        let dir_mode = std::fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o700, "dir mode is {dir_mode:o}");
        // No temp files left behind.
        let leftovers: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .map(|n| n.ends_with(".tmp"))
                    .unwrap_or(false)
            })
            .collect();
        assert!(leftovers.is_empty(), "temp files left: {leftovers:?}");
    }

    #[test]
    fn expiry_compares_against_now() {
        let t = sample();
        assert!(!t.is_expired_at(1_000_000));
        assert!(t.is_expired_at(1_800_000_000));
        assert!(t.is_expired_at(1_800_000_000 - EXPIRY_SKEW_SECS));
        assert!(!t.is_expired_at(1_800_000_000 - EXPIRY_SKEW_SECS - 1));
    }

    #[test]
    fn path_prefers_ancientos_home() {
        let _guard = env_lock();
        let saved_home = std::env::var("ANCIENTOS_HOME").ok();
        let saved_h = std::env::var("HOME").ok();

        std::env::set_var("ANCIENTOS_HOME", "/tmp/example-home");
        assert_eq!(
            token_path().unwrap(),
            PathBuf::from("/tmp/example-home/x_tokens.json")
        );

        std::env::remove_var("ANCIENTOS_HOME");
        std::env::set_var("HOME", "/tmp/fake-user");
        assert_eq!(
            token_path().unwrap(),
            PathBuf::from(format!("/tmp/fake-user/{HOME_DIR_NAME}/{TOKENS_FILE_NAME}"))
        );

        match saved_home {
            Some(v) => std::env::set_var("ANCIENTOS_HOME", v),
            None => std::env::remove_var("ANCIENTOS_HOME"),
        }
        match saved_h {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
    }

    #[test]
    fn missing_home_fails_closed() {
        let _guard = env_lock();
        let saved_home = std::env::var("ANCIENTOS_HOME").ok();
        let saved_h = std::env::var("HOME").ok();
        std::env::remove_var("ANCIENTOS_HOME");
        std::env::remove_var("HOME");
        assert!(matches!(token_path(), Err(TokenStoreError::NoHome)));

        match saved_home {
            Some(v) => std::env::set_var("ANCIENTOS_HOME", v),
            None => std::env::remove_var("ANCIENTOS_HOME"),
        }
        match saved_h {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
    }

    #[test]
    fn save_and_load_use_ancientos_home() {
        let _guard = env_lock();
        let saved = std::env::var("ANCIENTOS_HOME").ok();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("ANCIENTOS_HOME", dir.path());

        let path = save(&sample()).unwrap();
        assert_eq!(path, dir.path().join(TOKENS_FILE_NAME));
        assert_eq!(load().unwrap().access_token, "access-123");

        match saved {
            Some(v) => std::env::set_var("ANCIENTOS_HOME", v),
            None => std::env::remove_var("ANCIENTOS_HOME"),
        }
    }
}
