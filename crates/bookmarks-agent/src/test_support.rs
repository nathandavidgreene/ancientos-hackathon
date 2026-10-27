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
        ];
        let saved = keys.iter().map(|k| (*k, std::env::var(k).ok())).collect();
        (lock, saved)
    }

    /// Paid switch on, offline/cache overrides cleared.
    pub(crate) fn paid_on() -> Self {
        let (lock, saved) = Self::capture();
        std::env::set_var("ANCIENTOS_ALLOW_PAID", "1");
        std::env::remove_var(crate::liveness::OFFLINE_ENV_VAR);
        std::env::remove_var(crate::liveness::CACHE_ENV_VAR);
        Self { _lock: lock, saved }
    }

    /// Paid switch off (plus offline/cache overrides cleared): liveness
    /// skips deterministically with no network and no cache writes.
    pub(crate) fn paid_off() -> Self {
        let (lock, saved) = Self::capture();
        std::env::remove_var("ANCIENTOS_ALLOW_PAID");
        std::env::remove_var(crate::liveness::OFFLINE_ENV_VAR);
        std::env::remove_var(crate::liveness::CACHE_ENV_VAR);
        Self { _lock: lock, saved }
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
