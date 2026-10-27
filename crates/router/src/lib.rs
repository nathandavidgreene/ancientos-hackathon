//! Privacy router: sensitive data stays on the local Llama backend
//! (Ollama / llama.cpp via `ANCIENTOS_LOCAL_LLM_URL`); only sanitized data
//! may reach hosted Muse models via the Meta Model API. Endpoints and keys
//! come from env vars; paid calls are off by default.
use serde::Serialize;

pub mod dispatch;
pub mod jev_client;
pub mod local_llm;
pub mod modal;
pub mod muse;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Sensitivity {
    Sensitive,
    Sanitized,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Destination {
    LocalLlama,
    MuseSpark,
    MuseWonder,
    /// TypeSafe frontier API (hosted Jev). Sanitized/public content only;
    /// sensitive content must never route here.
    Jev,
    /// Local Jev server. Data stays on the device, so any sensitivity is OK.
    LocalJev,
}

#[derive(Debug, thiserror::Error)]
pub enum RouterError {
    #[error("missing env var {0}")]
    MissingEnv(&'static str),
    #[error("backend not implemented: {0:?}")]
    NotImplemented(Destination),
    #[error("paid calls are disabled (set ANCIENTOS_ALLOW_PAID=1 to enable)")]
    PaidCallsDisabled,
    #[error("http error: {0}")]
    Http(String),
    #[error("bad response: {0}")]
    BadResponse(String),
    #[error("refused: {0}")]
    Refused(String),
}

pub trait ModelClient {
    fn destination(&self) -> Destination;
    fn complete(&self, prompt: &str) -> Result<String, RouterError>;
    /// Sensitivity-aware completion. The default delegates to [`Self::complete`]
    /// (existing clients rely on route-then-call). [`jev_client::JevClient`]
    /// overrides this to refuse sensitive content on the TypeSafe backend.
    fn complete_with_sensitivity(
        &self,
        _sensitivity: Sensitivity,
        prompt: &str,
    ) -> Result<String, RouterError> {
        self.complete(prompt)
    }
}

/// Env var carrying the per-task Jev opt-in as a comma list
/// (`intent,bookmarks`). Unset or empty means Jev is off.
pub const ANCIENTOS_JEV_TASKS_ENV: &str = "ANCIENTOS_JEV_TASKS";

/// Tasks Jev may serve. Jev is OFF by default and is only used where an eval
/// (see `crates/eval`) shows it beats the alternatives on accuracy; a task
/// listed in [`JevTasks`] is the explicit opt-in for that task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum JevTask {
    /// Intent routing (`intent` in [`ANCIENTOS_JEV_TASKS_ENV`]).
    IntentRouting,
    /// Bookmark classification (`bookmarks` in [`ANCIENTOS_JEV_TASKS_ENV`]).
    BookmarkClassification,
}

impl JevTask {
    /// Canonical env-list name: `intent` or `bookmarks`.
    pub fn name(self) -> &'static str {
        match self {
            JevTask::IntentRouting => "intent",
            JevTask::BookmarkClassification => "bookmarks",
        }
    }

    /// Parse one env-list value (exact, case-sensitive). Unknown values are an
    /// error (fail closed), never silently ignored.
    pub fn parse(value: &str) -> Result<Self, RouterError> {
        match value.trim() {
            "intent" => Ok(JevTask::IntentRouting),
            "bookmarks" => Ok(JevTask::BookmarkClassification),
            other => Err(RouterError::Refused(format!(
                "unknown {ANCIENTOS_JEV_TASKS_ENV} value {other:?} (want intent,bookmarks)"
            ))),
        }
    }
}

impl std::str::FromStr for JevTask {
    type Err = RouterError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

/// Per-task Jev opt-in set. Empty (the default) means Jev is fully off:
/// routing never picks [`Destination::Jev`] or [`Destination::LocalJev`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JevTasks {
    tasks: std::collections::BTreeSet<JevTask>,
}

impl JevTasks {
    /// Empty set: Jev off for every task.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Set holding exactly `tasks` (duplicates collapse).
    pub fn of(tasks: &[JevTask]) -> Self {
        Self {
            tasks: tasks.iter().copied().collect(),
        }
    }

    pub fn contains(&self, task: JevTask) -> bool {
        self.tasks.contains(&task)
    }

    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    pub fn len(&self) -> usize {
        self.tasks.len()
    }

    /// Enabled tasks in canonical order.
    pub fn iter(&self) -> impl Iterator<Item = JevTask> + '_ {
        self.tasks.iter().copied()
    }

    /// Parse a comma list (`intent,bookmarks`). Empty or blank input is empty
    /// (off); blank segments are skipped; any other unknown value is an error
    /// (fail closed).
    pub fn from_comma_list(raw: &str) -> Result<Self, RouterError> {
        let mut tasks = std::collections::BTreeSet::new();
        for part in raw.split(',') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            tasks.insert(JevTask::parse(part)?);
        }
        Ok(Self { tasks })
    }

    /// Read [`ANCIENTOS_JEV_TASKS_ENV`]: unset, empty, or blank is off (empty);
    /// unknown values are an error (fail closed).
    pub fn from_env() -> Result<Self, RouterError> {
        match std::env::var(ANCIENTOS_JEV_TASKS_ENV)
            .ok()
            .filter(|v| !v.trim().is_empty())
        {
            None => Ok(Self::empty()),
            Some(raw) => Self::from_comma_list(&raw),
        }
    }

    /// [`Self::from_env`] that fails closed to empty (Jev off) instead of
    /// surfacing a config error. Prefer [`Self::from_env`] or
    /// [`RouterConfig::try_from_env`] where a misconfiguration must be visible.
    pub fn from_env_or_empty() -> Self {
        Self::from_env().unwrap_or_default()
    }
}

/// Config read from env: MODAL_LLAMA_URL, META_API_KEY, MUSE_MODEL,
/// ANCIENTOS_ALLOW_PAID, ANCIENTOS_JEV_TASKS. Never holds the key itself,
/// only whether it is set.
#[derive(Debug, Clone)]
pub struct RouterConfig {
    pub modal_llama_url: Option<String>,
    pub meta_api_key_set: bool,
    pub muse_model: String,
    pub allow_paid: bool,
    /// Per-task Jev opt-in. Empty by default (Jev off).
    pub jev_tasks: JevTasks,
}

impl RouterConfig {
    fn base_from_env() -> Self {
        let get = |k| std::env::var(k).ok().filter(|v: &String| !v.is_empty());
        Self {
            modal_llama_url: get("MODAL_LLAMA_URL"),
            meta_api_key_set: get("META_API_KEY").is_some(),
            muse_model: get("MUSE_MODEL").unwrap_or_else(|| muse::DEFAULT_MODEL.into()),
            allow_paid: std::env::var("ANCIENTOS_ALLOW_PAID")
                .map(|v| v == "1")
                .unwrap_or(false),
            jev_tasks: JevTasks::empty(),
        }
    }

    /// Read config from env. A malformed [`ANCIENTOS_JEV_TASKS_ENV`] fails
    /// closed to Jev-off; use [`Self::try_from_env`] to surface it as an error.
    pub fn from_env() -> Self {
        let mut cfg = Self::base_from_env();
        cfg.jev_tasks = JevTasks::from_env_or_empty();
        cfg
    }

    /// Read config from env, returning an error on unknown
    /// [`ANCIENTOS_JEV_TASKS_ENV`] values (fail closed) instead of silently
    /// disabling Jev.
    pub fn try_from_env() -> Result<Self, RouterError> {
        let mut cfg = Self::base_from_env();
        cfg.jev_tasks = JevTasks::from_env()?;
        Ok(cfg)
    }

    /// Whether Jev is opted in for `task`.
    pub fn jev_enabled_for(&self, task: JevTask) -> bool {
        self.jev_tasks.contains(task)
    }
}

/// Decide where data may go. Unknown sensitivity is treated as sensitive.
pub fn route(s: Sensitivity, wants_creative: bool) -> Destination {
    match s {
        Sensitivity::Sanitized if wants_creative => Destination::MuseWonder,
        Sensitivity::Sanitized => Destination::MuseSpark,
        Sensitivity::Sensitive | Sensitivity::Unknown => Destination::LocalLlama,
    }
}

/// Opt-in Jev routing. When `prefer_jev` is set, sanitized content routes to
/// the TypeSafe frontier backend ([`Destination::Jev`]) and sensitive/unknown
/// content routes to the local Jev server ([`Destination::LocalJev`]) — never
/// to TypeSafe. Without the preference this is exactly [`route`].
pub fn route_with_jev(s: Sensitivity, wants_creative: bool, prefer_jev: bool) -> Destination {
    if !prefer_jev {
        return route(s, wants_creative);
    }
    match s {
        Sensitivity::Sanitized => Destination::Jev,
        Sensitivity::Sensitive | Sensitivity::Unknown => Destination::LocalJev,
    }
}

/// Task-scoped Jev opt-in: routing never picks [`Destination::Jev`] unless
/// `task` is enabled in `jev_tasks` (Jev off by default). When enabled,
/// sanitized content routes to the TypeSafe frontier backend
/// ([`Destination::Jev`]) and sensitive/unknown content routes to the local
/// Jev server ([`Destination::LocalJev`]) — sensitive content still never
/// reaches TypeSafe. Without the opt-in this is exactly [`route`].
pub fn route_for_task(
    s: Sensitivity,
    wants_creative: bool,
    task: JevTask,
    jev_tasks: &JevTasks,
) -> Destination {
    route_with_jev(s, wants_creative, jev_tasks.contains(task))
}

/// Serialize tests that mutate process env vars. Every router test that
/// touches env must hold this guard; per-module mutexes do not exclude each
/// other and `ANCIENTOS_ALLOW_PAID` is shared across modules.
#[cfg(test)]
pub(crate) fn test_env_lock() -> std::sync::MutexGuard<'static, ()> {
    static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());
    ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner())
}

/// Stub client: returns NotImplemented until wired to a real endpoint.
pub struct StubClient(pub Destination);

impl ModelClient for StubClient {
    fn destination(&self) -> Destination {
        self.0
    }
    fn complete(&self, _prompt: &str) -> Result<String, RouterError> {
        Err(RouterError::NotImplemented(self.0))
    }
}

/// Deterministic mock for tests and offline demos.
pub struct MockClient(pub Destination);

impl ModelClient for MockClient {
    fn destination(&self) -> Destination {
        self.0
    }
    fn complete(&self, prompt: &str) -> Result<String, RouterError> {
        Ok(format!("[{:?}] {}", self.0, prompt.len()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_stays_local() {
        assert_eq!(route(Sensitivity::Unknown, true), Destination::LocalLlama);
        assert_eq!(
            route(Sensitivity::Sensitive, false),
            Destination::LocalLlama
        );
    }

    #[test]
    fn sanitized_goes_to_muse() {
        assert_eq!(route(Sensitivity::Sanitized, false), Destination::MuseSpark);
        assert_eq!(route(Sensitivity::Sanitized, true), Destination::MuseWonder);
    }

    #[test]
    fn stub_is_not_implemented() {
        assert!(StubClient(Destination::MuseSpark).complete("hi").is_err());
    }

    #[test]
    fn jev_opt_in_keeps_sensitive_off_typesafe() {
        for creative in [false, true] {
            assert_eq!(
                route_with_jev(Sensitivity::Sanitized, creative, true),
                Destination::Jev
            );
            // Sensitive and unknown stay on a local backend, never Jev.
            assert_eq!(
                route_with_jev(Sensitivity::Sensitive, creative, true),
                Destination::LocalJev
            );
            assert_eq!(
                route_with_jev(Sensitivity::Unknown, creative, true),
                Destination::LocalJev
            );
        }
    }

    #[test]
    fn jev_opt_out_is_plain_route() {
        for s in [
            Sensitivity::Sensitive,
            Sensitivity::Sanitized,
            Sensitivity::Unknown,
        ] {
            for creative in [false, true] {
                assert_eq!(route_with_jev(s, creative, false), route(s, creative));
            }
        }
    }

    const ALL_SENSITIVITIES: [Sensitivity; 3] = [
        Sensitivity::Sensitive,
        Sensitivity::Sanitized,
        Sensitivity::Unknown,
    ];
    const ALL_TASKS: [JevTask; 2] = [JevTask::IntentRouting, JevTask::BookmarkClassification];

    #[test]
    fn plain_route_never_picks_jev() {
        for s in ALL_SENSITIVITIES {
            for creative in [false, true] {
                let dest = route(s, creative);
                assert!(
                    !matches!(dest, Destination::Jev | Destination::LocalJev),
                    "route({s:?}, {creative}) must stay off Jev, got {dest:?}"
                );
            }
        }
    }

    #[test]
    fn jev_task_names_round_trip() {
        assert_eq!(JevTask::IntentRouting.name(), "intent");
        assert_eq!(JevTask::BookmarkClassification.name(), "bookmarks");
        for task in ALL_TASKS {
            assert_eq!(JevTask::parse(task.name()).unwrap(), task);
            assert_eq!(task.name().parse::<JevTask>().unwrap(), task);
        }
    }

    #[test]
    fn jev_tasks_comma_list_matrix() {
        // Empty / blank is off.
        for raw in ["", "   ", " , , "] {
            let tasks = JevTasks::from_comma_list(raw).expect("empty means off");
            assert!(tasks.is_empty(), "input {raw:?} must be empty");
            assert_eq!(tasks.len(), 0);
        }
        // Single tasks, whitespace tolerated, duplicates collapse.
        let intent = JevTasks::from_comma_list("intent").unwrap();
        assert_eq!(intent, JevTasks::of(&[JevTask::IntentRouting]));
        assert!(intent.contains(JevTask::IntentRouting));
        assert!(!intent.contains(JevTask::BookmarkClassification));
        let bookmarks = JevTasks::from_comma_list("  bookmarks ").unwrap();
        assert!(bookmarks.contains(JevTask::BookmarkClassification));
        assert!(!bookmarks.contains(JevTask::IntentRouting));
        let both = JevTasks::from_comma_list("intent,bookmarks").unwrap();
        assert_eq!(both.len(), 2);
        assert_eq!(
            both.iter().collect::<Vec<_>>(),
            vec![JevTask::IntentRouting, JevTask::BookmarkClassification]
        );
        let dupes = JevTasks::from_comma_list("bookmarks, intent,,bookmarks,").unwrap();
        assert_eq!(dupes, both);
        // Unknown values fail closed, never ignored or guessed.
        for raw in [
            "turbo",
            "intent,turbo",
            "Intent",
            "BOOKMARKS",
            "frontier",
            "jev",
            "all",
        ] {
            let err = JevTasks::from_comma_list(raw).unwrap_err();
            assert!(
                matches!(err, RouterError::Refused(_)),
                "input {raw:?} must refuse, got {err:?}"
            );
            assert!(err.to_string().contains(ANCIENTOS_JEV_TASKS_ENV));
        }
    }

    /// Set one process env var for a test, restoring the previous value on drop.
    struct EnvGuard {
        key: &'static str,
        saved: Option<String>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: Option<&str>) -> Self {
            let saved = std::env::var(key).ok();
            match value {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
            Self { key, saved }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match self.saved.take() {
                Some(v) => std::env::set_var(self.key, v),
                None => std::env::remove_var(self.key),
            }
        }
    }

    #[test]
    fn jev_tasks_from_env_matrix() {
        let _guard = test_env_lock();
        // Unset / empty / blank is off.
        for value in [None, Some(""), Some("   ")] {
            let _env = EnvGuard::set(ANCIENTOS_JEV_TASKS_ENV, value);
            assert_eq!(JevTasks::from_env().unwrap(), JevTasks::empty());
            assert_eq!(JevTasks::from_env_or_empty(), JevTasks::empty());
        }
        let _env = EnvGuard::set(ANCIENTOS_JEV_TASKS_ENV, Some("intent,bookmarks"));
        let tasks = JevTasks::from_env().unwrap();
        assert!(tasks.contains(JevTask::IntentRouting));
        assert!(tasks.contains(JevTask::BookmarkClassification));
        // Unknown fails closed: from_env errors, or_empty stays off.
        let _env = EnvGuard::set(ANCIENTOS_JEV_TASKS_ENV, Some("intent,bogus"));
        assert!(matches!(JevTasks::from_env(), Err(RouterError::Refused(_))));
        assert_eq!(JevTasks::from_env_or_empty(), JevTasks::empty());
    }

    #[test]
    fn router_config_jev_off_by_default() {
        let _guard = test_env_lock();
        let _env = EnvGuard::set(ANCIENTOS_JEV_TASKS_ENV, None);
        let cfg = RouterConfig::from_env();
        assert!(cfg.jev_tasks.is_empty());
        for task in ALL_TASKS {
            assert!(!cfg.jev_enabled_for(task));
        }
        let cfg = RouterConfig::try_from_env().unwrap();
        assert!(cfg.jev_tasks.is_empty());

        let _env = EnvGuard::set(ANCIENTOS_JEV_TASKS_ENV, Some("bookmarks"));
        let cfg = RouterConfig::from_env();
        assert!(cfg.jev_enabled_for(JevTask::BookmarkClassification));
        assert!(!cfg.jev_enabled_for(JevTask::IntentRouting));

        // Malformed list: from_env fails closed to off, try_from_env errors.
        let _env = EnvGuard::set(ANCIENTOS_JEV_TASKS_ENV, Some("bogus"));
        assert!(RouterConfig::from_env().jev_tasks.is_empty());
        assert!(matches!(
            RouterConfig::try_from_env(),
            Err(RouterError::Refused(_))
        ));
    }

    #[test]
    fn route_for_task_gates_jev_per_task() {
        let off = JevTasks::empty();
        // Off (or opted in for the OTHER task only): exactly plain route,
        // never a Jev destination.
        for s in ALL_SENSITIVITIES {
            for creative in [false, true] {
                for task in ALL_TASKS {
                    assert_eq!(route_for_task(s, creative, task, &off), route(s, creative));
                    let other = if task == JevTask::IntentRouting {
                        JevTask::BookmarkClassification
                    } else {
                        JevTask::IntentRouting
                    };
                    assert_eq!(
                        route_for_task(s, creative, task, &JevTasks::of(&[other])),
                        route(s, creative),
                        "opt-in for {other:?} must not enable {task:?}"
                    );
                }
            }
        }
        // Opted in for THIS task: sanitized to Jev (TypeSafe), sensitive and
        // unknown to localjev — sensitive still never reaches TypeSafe.
        for task in ALL_TASKS {
            let on = JevTasks::of(&[task]);
            for creative in [false, true] {
                assert_eq!(
                    route_for_task(Sensitivity::Sanitized, creative, task, &on),
                    Destination::Jev
                );
                assert_eq!(
                    route_for_task(Sensitivity::Sensitive, creative, task, &on),
                    Destination::LocalJev
                );
                assert_eq!(
                    route_for_task(Sensitivity::Unknown, creative, task, &on),
                    Destination::LocalJev
                );
            }
        }
    }
}
