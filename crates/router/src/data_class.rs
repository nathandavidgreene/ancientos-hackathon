//! Data classification with per-route ceilings.
//!
//! Every piece of content has a [`DataClass`]; every route has a ceiling, and
//! a call whose class exceeds the route ceiling is refused before any network
//! I/O ([`check_egress`]). Local routes ([`Route::LocalLlama`],
//! [`Route::LocalJev`]) may see everything; hosted routes
//! ([`Route::MuseSpark`], [`Route::MuseWonder`], [`Route::Jev`],
//! [`Route::Modal`]) accept [`DataClass::Public`] only.
//!
//! Fail closed everywhere: unclassified or unknown input classifies as
//! [`DataClass::Restricted`], which no hosted route may receive.
use crate::{RouterError, Sensitivity};
use ancientos_core::log::DecisionLog;

/// Classification of content, ordered from least to most sensitive:
///
/// `Public < Internal < Confidential < Restricted`.
///
/// Content built from several sources takes the highest class among them
/// (see [`max_of`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DataClass {
    /// Public: safe to show anyone. Ask: "could this go on a billboard?"
    /// ```rust
    /// # use ancientos_router::data_class::DataClass;
    /// assert!(DataClass::Public < DataClass::Internal);
    /// ```
    Public,
    /// Internal: fine on this device, not for hosted models. Ask: "would you
    /// email this to a stranger?" (Reserved: no classifier emits it today.)
    /// ```rust
    /// # use ancientos_router::data_class::DataClass;
    /// assert!(DataClass::Internal < DataClass::Confidential);
    /// ```
    Internal,
    /// Confidential: would hurt someone if it leaked. Ask: "would a leak of
    /// this harm a person or their privacy?"
    /// ```rust
    /// # use ancientos_router::data_class::DataClass;
    /// assert!(DataClass::Confidential < DataClass::Restricted);
    /// ```
    Confidential,
    /// Restricted: secrets and the unknown. Ask: "if this showed up in
    /// someone else's logs, would something need rotating or would it hurt a
    /// person?"
    /// ```rust
    /// # use ancientos_router::data_class::DataClass;
    /// assert_eq!(DataClass::parse("bogus-class"), DataClass::Restricted);
    /// ```
    Restricted,
}

impl DataClass {
    /// Stable lowercase name for logs and env values.
    pub fn as_str(self) -> &'static str {
        match self {
            DataClass::Public => "public",
            DataClass::Internal => "internal",
            DataClass::Confidential => "confidential",
            DataClass::Restricted => "restricted",
        }
    }

    /// Parse a class name (exact, lowercase). Unknown, empty, or malformed
    /// input is [`DataClass::Restricted`] (fail closed): unclassified content
    /// must never reach a hosted route.
    pub fn parse(raw: &str) -> Self {
        match raw.trim() {
            "public" => DataClass::Public,
            "internal" => DataClass::Internal,
            "confidential" => DataClass::Confidential,
            "restricted" => DataClass::Restricted,
            _ => DataClass::Restricted,
        }
    }
}

impl std::fmt::Display for DataClass {
    /// Route name.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for DataClass {
    type Err = std::convert::Infallible;
    /// Total parse: unknown input is [`DataClass::Restricted`] (fail closed).
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self::parse(s))
    }
}

impl From<Sensitivity> for DataClass {
    /// Bridge from the legacy sensitivity: sanitized public content is
    /// [`DataClass::Public`], sensitive content is [`DataClass::Confidential`],
    /// and unknown content is [`DataClass::Restricted`] (fail closed).
    fn from(s: Sensitivity) -> Self {
        match s {
            Sensitivity::Sanitized => DataClass::Public,
            Sensitivity::Sensitive => DataClass::Confidential,
            Sensitivity::Unknown => DataClass::Restricted,
        }
    }
}

/// A model route: where content would be sent. [`Destination`](crate::Destination)
/// cannot serve this role because the Modal client declares
/// `Destination::LocalLlama` (its dispatch slot) while actually being a
/// hosted, paid route with a hosted ceiling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Route {
    /// Local Llama server: data stays on the device.
    LocalLlama,
    /// Local Jev server: data stays on the device.
    LocalJev,
    /// Hosted Muse Spark via the Meta Model API.
    MuseSpark,
    /// Hosted Muse Wonder (creative overflow).
    MuseWonder,
    /// Hosted Jev via the TypeSafe frontier API.
    Jev,
    /// Modal-hosted Llama (hosted, paid).
    Modal,
}

impl Route {
    /// Stable snake_case name for logs.
    pub fn as_str(self) -> &'static str {
        match self {
            Route::LocalLlama => "local_llama",
            Route::LocalJev => "local_jev",
            Route::MuseSpark => "muse_spark",
            Route::MuseWonder => "muse_wonder",
            Route::Jev => "jev",
            Route::Modal => "modal",
        }
    }

    /// Map a dispatch destination to its route. Callers holding a Modal
    /// client must use [`Route::Modal`] directly: Modal declares
    /// `Destination::LocalLlama` as its dispatch slot, which would map here
    /// to the local ceiling.
    pub fn from_destination(d: crate::Destination) -> Self {
        match d {
            crate::Destination::LocalLlama => Route::LocalLlama,
            crate::Destination::MuseSpark => Route::MuseSpark,
            crate::Destination::MuseWonder => Route::MuseWonder,
            crate::Destination::Jev => Route::Jev,
            crate::Destination::LocalJev => Route::LocalJev,
        }
    }

    /// True for routes that send content off the device (and therefore log
    /// egress decisions). Local routes carry no egress entries.
    pub fn is_hosted(self) -> bool {
        !matches!(self, Route::LocalLlama | Route::LocalJev)
    }
}

impl std::fmt::Display for Route {
    /// Route name.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The single ceiling table: the highest class each route may receive.
/// Local routes may see everything ([`DataClass::Restricted`]); hosted routes
/// accept [`DataClass::Public`] only.
pub fn ceiling(route: Route) -> DataClass {
    match route {
        Route::LocalLlama | Route::LocalJev => DataClass::Restricted,
        Route::MuseSpark | Route::MuseWonder | Route::Jev | Route::Modal => DataClass::Public,
    }
}

/// Env override for the Muse hosted ceilings ([`Route::MuseSpark`],
/// [`Route::MuseWonder`]). See [`ceiling_from_env`] for why it can only lower.
pub const CEILING_MUSE_ENV: &str = "ANCIENTOS_CEILING_MUSE";
/// Env override for the hosted Jev ceiling ([`Route::Jev`]).
pub const CEILING_JEV_ENV: &str = "ANCIENTOS_CEILING_JEV";
/// Env override for the Modal ceiling ([`Route::Modal`]).
pub const CEILING_MODAL_ENV: &str = "ANCIENTOS_CEILING_MODAL";

/// Env var carrying a ceiling override for `route`, if any. Local routes
/// have none: their data stays on the device, so there is no ceiling to lower.
fn ceiling_env_var(route: Route) -> Option<&'static str> {
    match route {
        Route::MuseSpark | Route::MuseWonder => Some(CEILING_MUSE_ENV),
        Route::Jev => Some(CEILING_JEV_ENV),
        Route::Modal => Some(CEILING_MODAL_ENV),
        Route::LocalLlama | Route::LocalJev => None,
    }
}

/// Strict ceiling value: exact lowercase class names only.
fn parse_ceiling_value(raw: &str) -> Option<DataClass> {
    match raw.trim() {
        "public" => Some(DataClass::Public),
        "internal" => Some(DataClass::Internal),
        "confidential" => Some(DataClass::Confidential),
        "restricted" => Some(DataClass::Restricted),
        _ => None,
    }
}

/// Effective ceiling for `route`: the compiled [`ceiling`] clamped by an
/// optional env override that may only LOWER it, never raise it.
///
/// Why: env is low-trust configuration (typos, shared shells, CI injection).
/// A raised hosted ceiling would silently exfiltrate Confidential/Restricted
/// content to a hosted model, so the override resolves to
/// `min(compiled, parsed)` and unknown values fall back to the compiled
/// default. Hosted defaults are already [`DataClass::Public`] (the minimum),
/// so today env cannot move them at all; the clamp keeps that true even if a
/// default ever changes.
pub fn ceiling_from_env(route: Route) -> DataClass {
    let def = ceiling(route);
    let Some(var) = ceiling_env_var(route) else {
        return def;
    };
    let raw = std::env::var(var).ok().filter(|v| !v.trim().is_empty());
    let Some(raw) = raw else {
        return def;
    };
    parse_ceiling_value(&raw).map(|v| v.min(def)).unwrap_or(def)
}

/// Highest class among several sources. Content built from several sources
/// (post text plus enrichment, thread plus quotes) takes the maximum, so one
/// Restricted source taints the whole. Empty input is
/// [`DataClass::Restricted`] (no sources means unknown: fail closed).
pub fn max_of(classes: impl IntoIterator<Item = DataClass>) -> DataClass {
    classes.into_iter().max().unwrap_or(DataClass::Restricted)
}

/// Decision-log kind for a refused hosted call. Payload: class, route, caller
/// (never content).
pub const EGRESS_DENIED_KIND: &str = "router.egress_denied";
/// Decision-log kind for an allowed hosted call. Payload: class and route
/// only (never content, not even the caller).
pub const EGRESS_ALLOWED_KIND: &str = "router.egress_allowed";

/// Denied-entry payload: class, route, caller. No content, ever.
pub fn denied_payload(class: DataClass, route: Route, caller: &str) -> serde_json::Value {
    serde_json::json!({
        "class": class.as_str(),
        "route": route.as_str(),
        "caller": caller,
    })
}

/// Allowed-entry payload: class and route only.
pub fn allowed_payload(class: DataClass, route: Route) -> serde_json::Value {
    serde_json::json!({
        "class": class.as_str(),
        "route": route.as_str(),
    })
}

/// Enforce the ceiling for one call: refuse with
/// [`RouterError::EgressDenied`] when `class` exceeds the effective ceiling
/// for `route`, else allow. When `log` is given and the route is hosted, the
/// decision is appended (`router.egress_denied` with class/route/caller,
/// `router.egress_allowed` with class/route); a log failure fails closed (the
/// call is refused). Local routes never append: staying on the device is not
/// egress. Neither payload carries content.
pub fn check_egress(
    class: DataClass,
    route: Route,
    caller: &str,
    log: Option<&mut DecisionLog>,
) -> Result<(), RouterError> {
    let ceiling = ceiling_from_env(route);
    if class <= ceiling {
        if route.is_hosted() {
            if let Some(log) = log {
                log.append(EGRESS_ALLOWED_KIND, allowed_payload(class, route))
                    .map_err(|e| {
                        RouterError::Refused(format!("decision log append failed: {e}"))
                    })?;
            }
        }
        return Ok(());
    }
    if route.is_hosted() {
        if let Some(log) = log {
            if let Err(e) = log.append(EGRESS_DENIED_KIND, denied_payload(class, route, caller)) {
                return Err(RouterError::Refused(format!(
                    "decision log append failed: {e}"
                )));
            }
        }
    }
    Err(RouterError::EgressDenied {
        class,
        route,
        ceiling,
        caller: caller.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordering_is_public_internal_confidential_restricted() {
        assert!(DataClass::Public < DataClass::Internal);
        assert!(DataClass::Internal < DataClass::Confidential);
        assert!(DataClass::Confidential < DataClass::Restricted);
        let mut classes = [
            DataClass::Restricted,
            DataClass::Public,
            DataClass::Confidential,
            DataClass::Internal,
        ];
        classes.sort();
        assert_eq!(
            classes,
            [
                DataClass::Public,
                DataClass::Internal,
                DataClass::Confidential,
                DataClass::Restricted,
            ]
        );
    }

    #[test]
    fn parse_round_trips_and_unknown_is_restricted() {
        for class in [
            DataClass::Public,
            DataClass::Internal,
            DataClass::Confidential,
            DataClass::Restricted,
        ] {
            assert_eq!(DataClass::parse(class.as_str()), class);
            assert_eq!(class.as_str().parse::<DataClass>().unwrap(), class);
        }
        // Unknown, empty, or wrong-case input fails closed to Restricted.
        for raw in [
            "",
            "   ",
            "bogus",
            "Public",
            "PUBLIC",
            "top-secret",
            "sanitized",
            "sensitive",
            "internal!",
        ] {
            assert_eq!(
                DataClass::parse(raw),
                DataClass::Restricted,
                "input {raw:?}"
            );
        }
        // Surrounding whitespace is trimmed, then the match is exact.
        assert_eq!(DataClass::parse("  public\t"), DataClass::Public);
        assert_eq!("nope".parse::<DataClass>().unwrap(), DataClass::Restricted);
    }

    #[test]
    fn sensitivity_bridge_maps_unknown_to_restricted() {
        assert_eq!(DataClass::from(Sensitivity::Sanitized), DataClass::Public);
        assert_eq!(
            DataClass::from(Sensitivity::Sensitive),
            DataClass::Confidential
        );
        assert_eq!(DataClass::from(Sensitivity::Unknown), DataClass::Restricted);
    }

    #[test]
    fn ceiling_table_local_restricted_hosted_public() {
        assert_eq!(ceiling(Route::LocalLlama), DataClass::Restricted);
        assert_eq!(ceiling(Route::LocalJev), DataClass::Restricted);
        for route in [
            Route::MuseSpark,
            Route::MuseWonder,
            Route::Jev,
            Route::Modal,
        ] {
            assert_eq!(ceiling(route), DataClass::Public, "route {route:?}");
            assert!(route.is_hosted(), "route {route:?}");
        }
        assert!(!Route::LocalLlama.is_hosted());
        assert!(!Route::LocalJev.is_hosted());
    }

    #[test]
    fn route_maps_destinations() {
        assert_eq!(
            Route::from_destination(crate::Destination::LocalLlama),
            Route::LocalLlama
        );
        assert_eq!(
            Route::from_destination(crate::Destination::MuseSpark),
            Route::MuseSpark
        );
        assert_eq!(
            Route::from_destination(crate::Destination::MuseWonder),
            Route::MuseWonder
        );
        assert_eq!(Route::from_destination(crate::Destination::Jev), Route::Jev);
        assert_eq!(
            Route::from_destination(crate::Destination::LocalJev),
            Route::LocalJev
        );
    }

    /// Set process env vars for a test, restoring previous values on drop.
    struct EnvGuard {
        saved: Vec<(&'static str, Option<String>)>,
    }

    impl EnvGuard {
        /// Test helper: sets env vars while saving prior values.
        fn set(pairs: &[(&'static str, Option<&str>)]) -> Self {
            let mut saved = Vec::with_capacity(pairs.len());
            for (key, value) in pairs {
                saved.push((*key, std::env::var(key).ok()));
                match value {
                    Some(v) => std::env::set_var(key, v),
                    None => std::env::remove_var(key),
                }
            }
            Self { saved }
        }
    }

    impl Drop for EnvGuard {
        /// Test helper: restores saved env vars on drop.
        fn drop(&mut self) {
            for (key, value) in self.saved.drain(..) {
                match value {
                    Some(v) => std::env::set_var(key, v),
                    None => std::env::remove_var(key),
                }
            }
        }
    }

    #[test]
    fn env_cannot_raise_a_hosted_ceiling() {
        let _guard = crate::test_env_lock();
        let hosted = [
            (Route::MuseSpark, CEILING_MUSE_ENV),
            (Route::MuseWonder, CEILING_MUSE_ENV),
            (Route::Jev, CEILING_JEV_ENV),
            (Route::Modal, CEILING_MODAL_ENV),
        ];
        // Unset or blank: the compiled default.
        for (route, var) in hosted {
            for value in [None, Some(""), Some("   ")] {
                let _env = EnvGuard::set(&[(var, value)]);
                assert_eq!(ceiling_from_env(route), DataClass::Public);
            }
        }
        // Any value at or above Public (or garbage): still Public, never raised.
        for (route, var) in hosted {
            for value in [
                "public",
                "internal",
                "confidential",
                "restricted",
                "bogus",
                "Public",
                "RESTRICTED",
            ] {
                let _env = EnvGuard::set(&[(var, Some(value))]);
                assert_eq!(
                    ceiling_from_env(route),
                    DataClass::Public,
                    "route {route:?} value {value:?} must not raise"
                );
            }
        }
        // A hosted var never affects another route or a local route.
        let _env = EnvGuard::set(&[
            (CEILING_MUSE_ENV, Some("restricted")),
            (CEILING_JEV_ENV, Some("confidential")),
            (CEILING_MODAL_ENV, Some("internal")),
        ]);
        assert_eq!(ceiling_from_env(Route::MuseSpark), DataClass::Public);
        assert_eq!(ceiling_from_env(Route::Jev), DataClass::Public);
        assert_eq!(ceiling_from_env(Route::Modal), DataClass::Public);
        assert_eq!(ceiling_from_env(Route::LocalLlama), DataClass::Restricted);
        assert_eq!(ceiling_from_env(Route::LocalJev), DataClass::Restricted);
    }

    #[test]
    fn max_of_takes_the_highest_and_empty_is_restricted() {
        assert_eq!(max_of([]), DataClass::Restricted);
        assert_eq!(max_of([DataClass::Public]), DataClass::Public);
        assert_eq!(
            max_of([DataClass::Public, DataClass::Public]),
            DataClass::Public
        );
        assert_eq!(
            max_of([DataClass::Public, DataClass::Confidential]),
            DataClass::Confidential
        );
        assert_eq!(
            max_of([
                DataClass::Confidential,
                DataClass::Internal,
                DataClass::Public,
            ]),
            DataClass::Confidential
        );
        assert_eq!(
            max_of([DataClass::Public, DataClass::Restricted]),
            DataClass::Restricted
        );
    }

    #[test]
    fn allowed_check_logs_class_and_route_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("d.jsonl");
        let mut log = DecisionLog::open(&path).unwrap();
        check_egress(
            DataClass::Public,
            Route::MuseSpark,
            "test-caller",
            Some(&mut log),
        )
        .unwrap();
        let entries = log.verify().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].kind, EGRESS_ALLOWED_KIND);
        assert_eq!(entries[0].payload["class"], "public");
        assert_eq!(entries[0].payload["route"], "muse_spark");
        // Class and route only: no caller, no content.
        assert!(entries[0].payload.get("caller").is_none());
        let raw = serde_json::to_string(&entries[0].payload).unwrap();
        assert!(!raw.contains("test-caller"), "{raw}");
        assert!(!raw.contains("canary"), "{raw}");
    }

    #[test]
    fn denied_check_refuses_with_typed_error_and_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("d.jsonl");
        let mut log = DecisionLog::open(&path).unwrap();
        for class in [
            DataClass::Internal,
            DataClass::Confidential,
            DataClass::Restricted,
        ] {
            let err = check_egress(class, Route::Jev, "test-caller", Some(&mut log)).unwrap_err();
            assert!(
                matches!(
                    err,
                    RouterError::EgressDenied {
                        ceiling: DataClass::Public,
                        ..
                    }
                ),
                "unexpected: {err:?}"
            );
            // Typed error carries class/route/caller, never content.
            let msg = err.to_string();
            assert!(msg.contains("egress denied"), "{msg}");
            assert!(msg.contains("jev"), "{msg}");
            assert!(!msg.contains("canary"), "{msg}");
        }
        let entries = log.verify().unwrap();
        assert_eq!(entries.len(), 3);
        for e in &entries {
            assert_eq!(e.kind, EGRESS_DENIED_KIND);
            assert_eq!(e.payload["route"], "jev");
            assert_eq!(e.payload["caller"], "test-caller");
            assert!(e.payload.get("class").is_some());
            let raw = serde_json::to_string(&e.payload).unwrap();
            assert!(!raw.contains("canary"), "{raw}");
        }
        assert_eq!(entries[0].payload["class"], "internal");
        assert_eq!(entries[1].payload["class"], "confidential");
        assert_eq!(entries[2].payload["class"], "restricted");
    }

    #[test]
    fn local_routes_allow_everything_without_entries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("d.jsonl");
        let mut log = DecisionLog::open(&path).unwrap();
        for route in [Route::LocalLlama, Route::LocalJev] {
            for class in [
                DataClass::Public,
                DataClass::Internal,
                DataClass::Confidential,
                DataClass::Restricted,
            ] {
                check_egress(class, route, "test", Some(&mut log)).unwrap();
            }
        }
        // Allowed, and local calls are not egress: no entries even with a log.
        assert_eq!(log.verify_or_empty().unwrap().len(), 0);
    }

    #[test]
    fn check_without_log_still_enforces() {
        assert!(check_egress(DataClass::Public, Route::MuseSpark, "t", None).is_ok());
        assert!(matches!(
            check_egress(DataClass::Confidential, Route::MuseSpark, "t", None),
            Err(RouterError::EgressDenied { .. })
        ));
        assert!(matches!(
            check_egress(DataClass::Restricted, Route::Modal, "t", None),
            Err(RouterError::EgressDenied { .. })
        ));
        assert!(check_egress(DataClass::Restricted, Route::LocalLlama, "t", None).is_ok());
    }
}
