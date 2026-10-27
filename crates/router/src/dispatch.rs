//! Route-then-call: pick a destination with [`crate::route`] and call only
//! the client for that destination. Fails closed if a client's declared
//! destination does not match where the data is allowed to go. Sensitive
//! content routes to the local slot: wire it with
//! [`crate::local_llm::LocalLlmClient`] (default); [`crate::modal`] is an
//! optional future target, never a silent fallback to Muse.
//!
//! [`dispatch_with_jev`] adds the Jev opt-in: sanitized content goes to the
//! TypeSafe backend and sensitive/unknown content to the local Jev server.
//! Either Jev slot may be `None` (not wired); selecting it then is an error.
//! [`dispatch_for_task`] is the same opt-in scoped to one [`JevTask`]: Jev is
//! off by default and only an enabled task routes to it.
//!
//! The `*_with_class` twins take an explicit
//! [`DataClass`](crate::data_class::DataClass): a call whose class exceeds
//! the chosen route's ceiling is refused before the client is touched, with
//! a `router.egress_denied` entry when a log is given. The plain functions
//! derive the class from the sensitivity and log nothing.
use crate::data_class::{check_egress, DataClass, Route};
use crate::{
    route, route_with_jev, Destination, JevTask, JevTasks, ModelClient, RouterError, Sensitivity,
};
use ancientos_core::log::DecisionLog;

pub struct Backends<'a> {
    pub local: &'a dyn ModelClient,
    pub muse_spark: &'a dyn ModelClient,
    pub muse_wonder: &'a dyn ModelClient,
    /// TypeSafe frontier client (sanitized only). `None` = not wired.
    pub jev_typesafe: Option<&'a dyn ModelClient>,
    /// Local Jev server client. `None` = not wired.
    pub jev_local: Option<&'a dyn ModelClient>,
}

pub fn dispatch(
    b: &Backends<'_>,
    s: Sensitivity,
    wants_creative: bool,
    prompt: &str,
) -> Result<(Destination, String), RouterError> {
    dispatch_with_class(
        b,
        s,
        DataClass::from(s),
        wants_creative,
        "dispatch",
        None,
        prompt,
    )
}

/// Route-then-call with an explicit data class: the route comes from the
/// sensitivity (as in [`dispatch`]), then a class above the route ceiling is
/// refused before the client is touched. Hosted decisions are appended to
/// `log` when given; the client re-checks the class without logging.
pub fn dispatch_with_class(
    b: &Backends<'_>,
    s: Sensitivity,
    class: DataClass,
    wants_creative: bool,
    caller: &str,
    log: Option<&mut DecisionLog>,
    prompt: &str,
) -> Result<(Destination, String), RouterError> {
    let dest = route(s, wants_creative);
    let route = Route::from_destination(dest);
    if route.is_hosted() {
        check_egress(class, route, caller, log)?;
    }
    let client = match dest {
        Destination::LocalLlama => b.local,
        Destination::MuseSpark => b.muse_spark,
        Destination::MuseWonder => b.muse_wonder,
        // Unreachable: route() never yields a Jev destination. Plain dispatch
        // stays on the pre-Jev backends; use dispatch_with_jev to opt in.
        Destination::Jev | Destination::LocalJev => {
            return Err(RouterError::Refused(
                "plain dispatch never routes to Jev; use dispatch_with_jev".into(),
            ))
        }
    };
    if client.destination() != dest {
        return Err(RouterError::BadResponse(format!(
            "client for {dest:?} declares {:?}; refusing",
            client.destination()
        )));
    }
    Ok((
        dest,
        client.complete_with_class(class, caller, None, prompt)?,
    ))
}

/// Route-then-call with the Jev opt-in. When `prefer_jev` is set, sanitized
/// content goes to TypeSafe and sensitive/unknown content to the local Jev
/// server (never TypeSafe); otherwise this behaves exactly like [`dispatch`].
/// Selecting a Jev destination whose slot is `None` is an error (fail closed).
/// The call goes through the sensitivity-aware client method so a Jev client
/// double-checks the sensitivity even if it was mis-wired.
pub fn dispatch_with_jev(
    b: &Backends<'_>,
    s: Sensitivity,
    wants_creative: bool,
    prefer_jev: bool,
    prompt: &str,
) -> Result<(Destination, String), RouterError> {
    dispatch_with_jev_and_class(
        b,
        s,
        DataClass::from(s),
        wants_creative,
        prefer_jev,
        "dispatch_with_jev",
        None,
        prompt,
    )
}

/// [`dispatch_with_jev`] with an explicit data class: a class above the
/// chosen route's ceiling is refused before the client is touched, with a
/// `router.egress_denied` entry when a log is given.
#[allow(clippy::too_many_arguments)]
pub fn dispatch_with_jev_and_class(
    b: &Backends<'_>,
    s: Sensitivity,
    class: DataClass,
    wants_creative: bool,
    prefer_jev: bool,
    caller: &str,
    log: Option<&mut DecisionLog>,
    prompt: &str,
) -> Result<(Destination, String), RouterError> {
    let dest = route_with_jev(s, wants_creative, prefer_jev);
    let route = Route::from_destination(dest);
    if route.is_hosted() {
        check_egress(class, route, caller, log)?;
    }
    let client: &dyn ModelClient = match dest {
        Destination::LocalLlama => b.local,
        Destination::MuseSpark => b.muse_spark,
        Destination::MuseWonder => b.muse_wonder,
        Destination::Jev => b.jev_typesafe.ok_or_else(|| {
            RouterError::Refused("no TypeSafe backend wired for Destination::Jev".into())
        })?,
        Destination::LocalJev => b.jev_local.ok_or_else(|| {
            RouterError::Refused("no local-Jev backend wired for Destination::LocalJev".into())
        })?,
    };
    if client.destination() != dest {
        return Err(RouterError::BadResponse(format!(
            "client for {dest:?} declares {:?}; refusing",
            client.destination()
        )));
    }
    Ok((
        dest,
        client.complete_with_class(class, caller, None, prompt)?,
    ))
}

/// Route-then-call with the per-task Jev opt-in. Jev is off by default: when
/// `task` is not enabled in `jev_tasks` this behaves exactly like [`dispatch`]
/// and never touches a Jev backend. When enabled, sanitized content goes to
/// TypeSafe and sensitive/unknown content to the local Jev server (never
/// TypeSafe); selecting a Jev destination whose slot is `None` is an error.
pub fn dispatch_for_task(
    b: &Backends<'_>,
    s: Sensitivity,
    wants_creative: bool,
    task: JevTask,
    jev_tasks: &JevTasks,
    prompt: &str,
) -> Result<(Destination, String), RouterError> {
    dispatch_for_task_with_class(
        b,
        s,
        DataClass::from(s),
        wants_creative,
        task,
        jev_tasks,
        "dispatch_for_task",
        None,
        prompt,
    )
}

/// [`dispatch_for_task`] with an explicit data class: a class above the
/// chosen route's ceiling is refused before the client is touched, with a
/// `router.egress_denied` entry when a log is given.
#[allow(clippy::too_many_arguments)]
pub fn dispatch_for_task_with_class(
    b: &Backends<'_>,
    s: Sensitivity,
    class: DataClass,
    wants_creative: bool,
    task: JevTask,
    jev_tasks: &JevTasks,
    caller: &str,
    log: Option<&mut DecisionLog>,
    prompt: &str,
) -> Result<(Destination, String), RouterError> {
    dispatch_with_jev_and_class(
        b,
        s,
        class,
        wants_creative,
        jev_tasks.contains(task),
        caller,
        log,
        prompt,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// Records every prompt it receives.
    struct Recorder {
        dest: Destination,
        seen: RefCell<Vec<String>>,
    }
    impl Recorder {
        fn new(dest: Destination) -> Self {
            Self {
                dest,
                seen: RefCell::new(Vec::new()),
            }
        }
    }
    impl ModelClient for Recorder {
        fn destination(&self) -> Destination {
            self.dest
        }
        fn complete(&self, p: &str) -> Result<String, RouterError> {
            self.seen.borrow_mut().push(p.into());
            Ok("ok".into())
        }
    }

    #[test]
    fn sensitive_data_never_reaches_muse() {
        let local = Recorder::new(Destination::LocalLlama);
        let spark = Recorder::new(Destination::MuseSpark);
        let wonder = Recorder::new(Destination::MuseWonder);
        let b = Backends {
            local: &local,
            muse_spark: &spark,
            muse_wonder: &wonder,
            jev_typesafe: None,
            jev_local: None,
        };
        for s in [Sensitivity::Sensitive, Sensitivity::Unknown] {
            for creative in [false, true] {
                let (d, _) = dispatch(&b, s, creative, "PRIVATE health note").unwrap();
                assert_eq!(d, Destination::LocalLlama);
            }
        }
        dispatch(&b, Sensitivity::Sanitized, false, "public post").unwrap();
        assert_eq!(local.seen.borrow().len(), 4);
        assert!(spark.seen.borrow().iter().all(|p| !p.contains("PRIVATE")));
        assert!(wonder.seen.borrow().is_empty());
        assert_eq!(spark.seen.borrow().len(), 1);
    }

    #[test]
    fn mismatched_client_fails_closed() {
        // A Muse client wired into the local slot must not receive sensitive data.
        let wrong = Recorder::new(Destination::MuseSpark);
        let spark = Recorder::new(Destination::MuseSpark);
        let wonder = Recorder::new(Destination::MuseWonder);
        let b = Backends {
            local: &wrong,
            muse_spark: &spark,
            muse_wonder: &wonder,
            jev_typesafe: None,
            jev_local: None,
        };
        assert!(dispatch(&b, Sensitivity::Sensitive, false, "PRIVATE").is_err());
        assert!(wrong.seen.borrow().is_empty());
    }

    #[test]
    fn modal_stub_fails_closed_without_paid_switch() {
        let modal = crate::modal::ModalLlamaClient::from_lookup(|_| None);
        let spark = Recorder::new(Destination::MuseSpark);
        let wonder = Recorder::new(Destination::MuseWonder);
        let b = Backends {
            local: &modal,
            muse_spark: &spark,
            muse_wonder: &wonder,
            jev_typesafe: None,
            jev_local: None,
        };
        // Public content reaches the stub's own paid gate.
        assert!(matches!(
            dispatch_with_class(
                &b,
                Sensitivity::Sensitive,
                DataClass::Public,
                false,
                "test",
                None,
                "public",
            ),
            Err(RouterError::PaidCallsDisabled)
        ));
        // Non-public content is refused by the Modal ceiling first, even
        // though Modal sits in the local dispatch slot.
        assert!(matches!(
            dispatch(&b, Sensitivity::Sensitive, false, "PRIVATE"),
            Err(RouterError::EgressDenied { .. })
        ));
        assert!(spark.seen.borrow().is_empty());
    }

    fn jev_backends<'a>(
        local: &'a Recorder,
        spark: &'a Recorder,
        wonder: &'a Recorder,
        typesafe: &'a Recorder,
        local_jev: &'a Recorder,
    ) -> Backends<'a> {
        Backends {
            local,
            muse_spark: spark,
            muse_wonder: wonder,
            jev_typesafe: Some(typesafe),
            jev_local: Some(local_jev),
        }
    }

    #[test]
    fn jev_opt_in_sends_sanitized_to_typesafe_only() {
        let local = Recorder::new(Destination::LocalLlama);
        let spark = Recorder::new(Destination::MuseSpark);
        let wonder = Recorder::new(Destination::MuseWonder);
        let typesafe = Recorder::new(Destination::Jev);
        let local_jev = Recorder::new(Destination::LocalJev);
        let b = jev_backends(&local, &spark, &wonder, &typesafe, &local_jev);
        let (d, _) =
            dispatch_with_jev(&b, Sensitivity::Sanitized, false, true, "public post").unwrap();
        assert_eq!(d, Destination::Jev);
        assert_eq!(typesafe.seen.borrow().len(), 1);
        assert!(local_jev.seen.borrow().is_empty());
        assert!(spark.seen.borrow().is_empty());
        assert!(wonder.seen.borrow().is_empty());
    }

    #[test]
    fn jev_opt_in_keeps_sensitive_off_typesafe() {
        let local = Recorder::new(Destination::LocalLlama);
        let spark = Recorder::new(Destination::MuseSpark);
        let wonder = Recorder::new(Destination::MuseWonder);
        let typesafe = Recorder::new(Destination::Jev);
        let local_jev = Recorder::new(Destination::LocalJev);
        let b = jev_backends(&local, &spark, &wonder, &typesafe, &local_jev);
        for s in [Sensitivity::Sensitive, Sensitivity::Unknown] {
            for creative in [false, true] {
                let (d, _) =
                    dispatch_with_jev(&b, s, creative, true, "PRIVATE health note").unwrap();
                assert_eq!(d, Destination::LocalJev);
            }
        }
        assert!(typesafe.seen.borrow().is_empty(), "typesafe saw sensitive");
        assert!(spark.seen.borrow().is_empty());
        assert!(wonder.seen.borrow().is_empty());
        assert_eq!(local_jev.seen.borrow().len(), 4);
    }

    #[test]
    fn jev_opt_out_behaves_like_plain_dispatch() {
        let local = Recorder::new(Destination::LocalLlama);
        let spark = Recorder::new(Destination::MuseSpark);
        let wonder = Recorder::new(Destination::MuseWonder);
        let typesafe = Recorder::new(Destination::Jev);
        let local_jev = Recorder::new(Destination::LocalJev);
        let b = jev_backends(&local, &spark, &wonder, &typesafe, &local_jev);
        let (d, _) = dispatch_with_jev(&b, Sensitivity::Sanitized, false, false, "public").unwrap();
        assert_eq!(d, Destination::MuseSpark);
        let (d, _) = dispatch_with_jev(&b, Sensitivity::Sensitive, true, false, "PRIVATE").unwrap();
        assert_eq!(d, Destination::LocalLlama);
        assert!(typesafe.seen.borrow().is_empty());
        assert!(local_jev.seen.borrow().is_empty());
    }

    #[test]
    fn unwired_jev_slot_fails_closed() {
        let local = Recorder::new(Destination::LocalLlama);
        let spark = Recorder::new(Destination::MuseSpark);
        let wonder = Recorder::new(Destination::MuseWonder);
        let b = Backends {
            local: &local,
            muse_spark: &spark,
            muse_wonder: &wonder,
            jev_typesafe: None,
            jev_local: None,
        };
        assert!(matches!(
            dispatch_with_jev(&b, Sensitivity::Sanitized, false, true, "public"),
            Err(RouterError::Refused(_))
        ));
        assert!(matches!(
            dispatch_with_jev(&b, Sensitivity::Sensitive, false, true, "PRIVATE"),
            Err(RouterError::Refused(_))
        ));
    }

    #[test]
    fn mismatched_jev_client_fails_closed() {
        // A Muse client wired into the TypeSafe slot must not be called.
        let local = Recorder::new(Destination::LocalLlama);
        let spark = Recorder::new(Destination::MuseSpark);
        let wonder = Recorder::new(Destination::MuseWonder);
        let wrong = Recorder::new(Destination::MuseSpark);
        let local_jev = Recorder::new(Destination::LocalJev);
        let b = Backends {
            local: &local,
            muse_spark: &spark,
            muse_wonder: &wonder,
            jev_typesafe: Some(&wrong),
            jev_local: Some(&local_jev),
        };
        assert!(dispatch_with_jev(&b, Sensitivity::Sanitized, false, true, "public").is_err());
        assert!(wrong.seen.borrow().is_empty());
    }

    #[test]
    fn real_jev_client_double_checks_sensitivity_in_dispatch() {
        use crate::jev_client::{JevBackend, JevClient};
        // A real TypeSafe client mis-wired into the local-Jev slot: the
        // destination guard catches it before any network use.
        let local = Recorder::new(Destination::LocalLlama);
        let spark = Recorder::new(Destination::MuseSpark);
        let wonder = Recorder::new(Destination::MuseWonder);
        let typesafe = JevClient::new(JevBackend::TypeSafe, "http://127.0.0.1:9", "m", true);
        let b = Backends {
            local: &local,
            muse_spark: &spark,
            muse_wonder: &wonder,
            jev_typesafe: Some(&typesafe),
            jev_local: Some(&typesafe),
        };
        // Sensitive dispatches to LocalJev, where the mis-wired client
        // declares Jev: refused without touching the network.
        assert!(dispatch_with_jev(&b, Sensitivity::Sensitive, false, true, "PRIVATE").is_err());
    }

    #[test]
    fn dispatch_for_task_off_is_plain_dispatch() {
        let local = Recorder::new(Destination::LocalLlama);
        let spark = Recorder::new(Destination::MuseSpark);
        let wonder = Recorder::new(Destination::MuseWonder);
        let typesafe = Recorder::new(Destination::Jev);
        let local_jev = Recorder::new(Destination::LocalJev);
        let b = jev_backends(&local, &spark, &wonder, &typesafe, &local_jev);
        // Empty set: Jev off even though both Jev slots are wired.
        let off = JevTasks::empty();
        let (d, _) = dispatch_for_task(
            &b,
            Sensitivity::Sanitized,
            false,
            JevTask::IntentRouting,
            &off,
            "public",
        )
        .unwrap();
        assert_eq!(d, Destination::MuseSpark);
        let (d, _) = dispatch_for_task(
            &b,
            Sensitivity::Sensitive,
            true,
            JevTask::BookmarkClassification,
            &off,
            "PRIVATE",
        )
        .unwrap();
        assert_eq!(d, Destination::LocalLlama);
        // Opt-in for the other task only: still plain dispatch.
        let intent_only = JevTasks::of(&[JevTask::IntentRouting]);
        let (d, _) = dispatch_for_task(
            &b,
            Sensitivity::Sanitized,
            false,
            JevTask::BookmarkClassification,
            &intent_only,
            "public",
        )
        .unwrap();
        assert_eq!(d, Destination::MuseSpark);
        assert!(typesafe.seen.borrow().is_empty());
        assert!(local_jev.seen.borrow().is_empty());
    }

    #[test]
    fn dispatch_for_task_on_routes_per_task() {
        let local = Recorder::new(Destination::LocalLlama);
        let spark = Recorder::new(Destination::MuseSpark);
        let wonder = Recorder::new(Destination::MuseWonder);
        let typesafe = Recorder::new(Destination::Jev);
        let local_jev = Recorder::new(Destination::LocalJev);
        let b = jev_backends(&local, &spark, &wonder, &typesafe, &local_jev);
        let on = JevTasks::of(&[JevTask::IntentRouting]);
        let (d, _) = dispatch_for_task(
            &b,
            Sensitivity::Sanitized,
            false,
            JevTask::IntentRouting,
            &on,
            "public post",
        )
        .unwrap();
        assert_eq!(d, Destination::Jev);
        for s in [Sensitivity::Sensitive, Sensitivity::Unknown] {
            let (d, _) = dispatch_for_task(
                &b,
                s,
                false,
                JevTask::IntentRouting,
                &on,
                "PRIVATE health note",
            )
            .unwrap();
            assert_eq!(d, Destination::LocalJev);
        }
        assert_eq!(typesafe.seen.borrow().len(), 1);
        assert_eq!(local_jev.seen.borrow().len(), 2);
        assert!(local.seen.borrow().is_empty());
        assert!(spark.seen.borrow().is_empty());
        assert!(wonder.seen.borrow().is_empty());
    }

    #[test]
    fn dispatch_for_task_unwired_jev_slot_fails_closed() {
        let local = Recorder::new(Destination::LocalLlama);
        let spark = Recorder::new(Destination::MuseSpark);
        let wonder = Recorder::new(Destination::MuseWonder);
        let b = Backends {
            local: &local,
            muse_spark: &spark,
            muse_wonder: &wonder,
            jev_typesafe: None,
            jev_local: None,
        };
        let on = JevTasks::of(&[JevTask::BookmarkClassification]);
        assert!(matches!(
            dispatch_for_task(
                &b,
                Sensitivity::Sanitized,
                false,
                JevTask::BookmarkClassification,
                &on,
                "public",
            ),
            Err(RouterError::Refused(_))
        ));
        // Off stays usable without any Jev wiring.
        let (d, _) = dispatch_for_task(
            &b,
            Sensitivity::Sanitized,
            false,
            JevTask::IntentRouting,
            &on,
            "public",
        )
        .unwrap();
        assert_eq!(d, Destination::MuseSpark);
    }

    fn plain_backends<'a>(
        local: &'a Recorder,
        spark: &'a Recorder,
        wonder: &'a Recorder,
    ) -> Backends<'a> {
        Backends {
            local,
            muse_spark: spark,
            muse_wonder: wonder,
            jev_typesafe: None,
            jev_local: None,
        }
    }

    #[test]
    fn over_ceiling_dispatch_refuses_before_any_client_with_denied_entry() {
        let local = Recorder::new(Destination::LocalLlama);
        let spark = Recorder::new(Destination::MuseSpark);
        let wonder = Recorder::new(Destination::MuseWonder);
        let b = plain_backends(&local, &spark, &wonder);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("d.jsonl");
        let mut log = DecisionLog::open(&path).unwrap();
        for class in [
            DataClass::Internal,
            DataClass::Confidential,
            DataClass::Restricted,
        ] {
            let err = dispatch_with_class(
                &b,
                Sensitivity::Sanitized,
                class,
                false,
                "test-dispatch",
                Some(&mut log),
                "canary prompt",
            )
            .unwrap_err();
            assert!(
                matches!(err, RouterError::EgressDenied { .. }),
                "unexpected: {err:?}"
            );
        }
        // No client saw the prompt.
        assert!(local.seen.borrow().is_empty());
        assert!(spark.seen.borrow().is_empty());
        assert!(wonder.seen.borrow().is_empty());
        let entries = log.verify().unwrap();
        assert_eq!(entries.len(), 3);
        for (e, class) in entries
            .iter()
            .zip(["internal", "confidential", "restricted"])
        {
            assert_eq!(e.kind, crate::data_class::EGRESS_DENIED_KIND);
            assert_eq!(e.payload["class"], class);
            assert_eq!(e.payload["route"], "muse_spark");
            assert_eq!(e.payload["caller"], "test-dispatch");
            let raw = serde_json::to_string(&e.payload).unwrap();
            assert!(!raw.contains("canary"), "{raw}");
        }
    }

    #[test]
    fn allowed_dispatch_logs_class_and_route_only() {
        let local = Recorder::new(Destination::LocalLlama);
        let spark = Recorder::new(Destination::MuseSpark);
        let wonder = Recorder::new(Destination::MuseWonder);
        let b = plain_backends(&local, &spark, &wonder);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("d.jsonl");
        let mut log = DecisionLog::open(&path).unwrap();
        let (d, _) = dispatch_with_class(
            &b,
            Sensitivity::Sanitized,
            DataClass::Public,
            false,
            "test-dispatch",
            Some(&mut log),
            "public post",
        )
        .unwrap();
        assert_eq!(d, Destination::MuseSpark);
        let (d, _) = dispatch_with_class(
            &b,
            Sensitivity::Sanitized,
            DataClass::Public,
            true,
            "test-dispatch",
            Some(&mut log),
            "public post",
        )
        .unwrap();
        assert_eq!(d, Destination::MuseWonder);
        assert_eq!(spark.seen.borrow().len(), 1);
        assert_eq!(wonder.seen.borrow().len(), 1);
        let entries = log.verify().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].kind, crate::data_class::EGRESS_ALLOWED_KIND);
        assert_eq!(entries[0].payload["class"], "public");
        assert_eq!(entries[0].payload["route"], "muse_spark");
        assert_eq!(entries[1].payload["route"], "muse_wonder");
        assert!(entries[0].payload.get("caller").is_none());
    }

    #[test]
    fn local_route_accepts_every_class_without_entries() {
        let local = Recorder::new(Destination::LocalLlama);
        let spark = Recorder::new(Destination::MuseSpark);
        let wonder = Recorder::new(Destination::MuseWonder);
        let b = plain_backends(&local, &spark, &wonder);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("d.jsonl");
        let mut log = DecisionLog::open(&path).unwrap();
        for s in [Sensitivity::Sensitive, Sensitivity::Unknown] {
            for class in [
                DataClass::Public,
                DataClass::Internal,
                DataClass::Confidential,
                DataClass::Restricted,
            ] {
                for creative in [false, true] {
                    let (d, _) = dispatch_with_class(
                        &b,
                        s,
                        class,
                        creative,
                        "test-dispatch",
                        Some(&mut log),
                        "anything",
                    )
                    .unwrap();
                    assert_eq!(d, Destination::LocalLlama);
                }
            }
        }
        assert_eq!(local.seen.borrow().len(), 16);
        assert!(spark.seen.borrow().is_empty());
        assert!(wonder.seen.borrow().is_empty());
        // Local calls are not egress: allowed, but never logged.
        assert_eq!(log.verify_or_empty().unwrap().len(), 0);
    }

    #[test]
    fn jev_dispatch_with_class_guards_typesafe() {
        let local = Recorder::new(Destination::LocalLlama);
        let spark = Recorder::new(Destination::MuseSpark);
        let wonder = Recorder::new(Destination::MuseWonder);
        let typesafe = Recorder::new(Destination::Jev);
        let local_jev = Recorder::new(Destination::LocalJev);
        let b = jev_backends(&local, &spark, &wonder, &typesafe, &local_jev);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("d.jsonl");
        let mut log = DecisionLog::open(&path).unwrap();
        // Confidential content routed Jev is refused before the client.
        assert!(matches!(
            dispatch_with_jev_and_class(
                &b,
                Sensitivity::Sanitized,
                DataClass::Confidential,
                false,
                true,
                "test-dispatch",
                Some(&mut log),
                "canary",
            ),
            Err(RouterError::EgressDenied { .. })
        ));
        assert!(typesafe.seen.borrow().is_empty());
        // Public content flows and logs one allowed entry.
        let (d, _) = dispatch_with_jev_and_class(
            &b,
            Sensitivity::Sanitized,
            DataClass::Public,
            false,
            true,
            "test-dispatch",
            Some(&mut log),
            "public",
        )
        .unwrap();
        assert_eq!(d, Destination::Jev);
        assert_eq!(typesafe.seen.borrow().len(), 1);
        // Local-Jev accepts everything without entries.
        let (d, _) = dispatch_with_jev_and_class(
            &b,
            Sensitivity::Sensitive,
            DataClass::Restricted,
            false,
            true,
            "test-dispatch",
            Some(&mut log),
            "PRIVATE",
        )
        .unwrap();
        assert_eq!(d, Destination::LocalJev);
        assert_eq!(local_jev.seen.borrow().len(), 1);
        let entries = log.verify().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].kind, crate::data_class::EGRESS_DENIED_KIND);
        assert_eq!(entries[0].payload["route"], "jev");
        assert_eq!(entries[1].kind, crate::data_class::EGRESS_ALLOWED_KIND);
        assert_eq!(entries[1].payload["route"], "jev");
    }

    #[test]
    fn task_dispatch_with_class_refuses_and_logs() {
        let local = Recorder::new(Destination::LocalLlama);
        let spark = Recorder::new(Destination::MuseSpark);
        let wonder = Recorder::new(Destination::MuseWonder);
        let typesafe = Recorder::new(Destination::Jev);
        let local_jev = Recorder::new(Destination::LocalJev);
        let b = jev_backends(&local, &spark, &wonder, &typesafe, &local_jev);
        let on = JevTasks::of(&[JevTask::BookmarkClassification]);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("d.jsonl");
        let mut log = DecisionLog::open(&path).unwrap();
        assert!(matches!(
            dispatch_for_task_with_class(
                &b,
                Sensitivity::Sanitized,
                DataClass::Restricted,
                false,
                JevTask::BookmarkClassification,
                &on,
                "test-dispatch",
                Some(&mut log),
                "canary",
            ),
            Err(RouterError::EgressDenied { .. })
        ));
        assert!(typesafe.seen.borrow().is_empty());
        assert_eq!(log.verify().unwrap().len(), 1);
    }
}
