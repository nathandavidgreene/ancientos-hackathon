//! Test-only shared env guard. Rust runs a crate's tests in threads sharing
//! one process env, so env-mutating tests must serialize on one crate-wide
//! mutex; per-module locks still race each other.
use std::sync::{Mutex, MutexGuard};

static ENV_MUTEX: Mutex<()> = Mutex::new(());

/// Hold the crate-wide env lock.
pub(crate) fn lock_env() -> MutexGuard<'static, ()> {
    ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner())
}

/// Holds the env lock and a paid/offline/cache setup, restoring the
/// previous values on drop.
pub(crate) struct EnvHold {
    _lock: MutexGuard<'static, ()>,
    saved: Vec<(&'static str, Option<String>)>,
}

impl EnvHold {
    fn capture() -> (MutexGuard<'static, ()>, Vec<(&'static str, Option<String>)>) {
        let lock = lock_env();
        let keys = [
            "ANCIENTOS_ALLOW_PAID",
            crate::liveness::OFFLINE_ENV_VAR,
            crate::liveness::CACHE_ENV_VAR,
            crate::link_context::FETCH_ENV_VAR,
            crate::link_context::CACHE_ENV_VAR,
            crate::link_context::ALLOW_PRIVATE_ENV_VAR,
            crate::link_context::WHISPER_CMD_ENV_VAR,
            crate::link_context::WHISPER_MODEL_ENV_VAR,
            crate::link_context::YTDLP_TIMEOUT_ENV_VAR,
            crate::link_context::FFMPEG_TIMEOUT_ENV_VAR,
            crate::link_context::WHISPER_TIMEOUT_ENV_VAR,
            crate::link_context::VIDEO_BASE_ENV_VAR,
            "FAKE_YTDLP_LOG",
            "FAKE_FFMPEG_LOG",
            "FAKE_WHISPER_LOG",
            // Captured for restore only; never cleared by the setups below.
            "PATH",
            "TMPDIR",
        ];
        let saved = keys.iter().map(|k| (*k, std::env::var(k).ok())).collect();
        (lock, saved)
    }

    fn clear_link_overrides() {
        std::env::remove_var(crate::liveness::OFFLINE_ENV_VAR);
        std::env::remove_var(crate::liveness::CACHE_ENV_VAR);
        std::env::remove_var(crate::link_context::FETCH_ENV_VAR);
        std::env::remove_var(crate::link_context::CACHE_ENV_VAR);
        std::env::remove_var(crate::link_context::ALLOW_PRIVATE_ENV_VAR);
        std::env::remove_var(crate::link_context::WHISPER_CMD_ENV_VAR);
        std::env::remove_var(crate::link_context::WHISPER_MODEL_ENV_VAR);
        std::env::remove_var(crate::link_context::YTDLP_TIMEOUT_ENV_VAR);
        std::env::remove_var(crate::link_context::FFMPEG_TIMEOUT_ENV_VAR);
        std::env::remove_var(crate::link_context::WHISPER_TIMEOUT_ENV_VAR);
        std::env::remove_var(crate::link_context::VIDEO_BASE_ENV_VAR);
        std::env::remove_var("FAKE_YTDLP_LOG");
        std::env::remove_var("FAKE_FFMPEG_LOG");
        std::env::remove_var("FAKE_WHISPER_LOG");
    }

    /// Paid switch on, offline/cache/link-fetch overrides cleared.
    pub(crate) fn paid_on() -> Self {
        let (lock, saved) = Self::capture();
        std::env::set_var("ANCIENTOS_ALLOW_PAID", "1");
        Self::clear_link_overrides();
        Self { _lock: lock, saved }
    }

    /// Paid switch off (plus offline/cache overrides cleared): liveness
    /// skips deterministically with no network and no cache writes.
    pub(crate) fn paid_off() -> Self {
        let (lock, saved) = Self::capture();
        std::env::remove_var("ANCIENTOS_ALLOW_PAID");
        Self::clear_link_overrides();
        Self { _lock: lock, saved }
    }

    /// Link fetching on (plus paid on, plus private hosts allowed for
    /// 127.0.0.1 mock servers): [`crate::link_context::enrich`] may fetch.
    /// Callers that need paid off with fetching on set
    /// `ANCIENTOS_FETCH_LINKS=1` themselves on top of [`Self::paid_off`].
    /// Callers testing private-host refusal unset
    /// `LINK_CONTEXT_ALLOW_PRIVATE_HOSTS` after this setup.
    pub(crate) fn fetch_on() -> Self {
        let hold = Self::paid_on();
        std::env::set_var(crate::link_context::FETCH_ENV_VAR, "1");
        std::env::set_var(crate::link_context::ALLOW_PRIVATE_ENV_VAR, "1");
        hold
    }
}

impl Drop for EnvHold {
    fn drop(&mut self) {
        for (key, value) in self.saved.drain(..) {
            match value {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        }
    }
}
