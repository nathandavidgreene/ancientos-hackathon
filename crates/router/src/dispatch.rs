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
use crate::{
    route, route_with_jev, Destination, JevTask, JevTasks, ModelClient, RouterError, Sensitivity,
};

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
    let dest = route(s, wants_creative);
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
    Ok((dest, client.complete(prompt)?))
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
    let dest = route_with_jev(s, wants_creative, prefer_jev);
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
    Ok((dest, client.complete_with_sensitivity(s, prompt)?))
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
    dispatch_with_jev(b, s, wants_creative, jev_tasks.contains(task), prompt)
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
        assert!(matches!(
            dispatch(&b, Sensitivity::Sensitive, false, "PRIVATE"),
            Err(RouterError::PaidCallsDisabled)
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
}
