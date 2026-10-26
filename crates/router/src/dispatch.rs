//! Route-then-call: pick a destination with [`crate::route`] and call only
//! the client for that destination. Fails closed if a client's declared
//! destination does not match where the data is allowed to go. Sensitive
//! content routes to the local slot: wire it with
//! [`crate::local_llm::LocalLlmClient`] (default); [`crate::modal`] is an
//! optional future target, never a silent fallback to Muse.
use crate::{route, Destination, ModelClient, RouterError, Sensitivity};

pub struct Backends<'a> {
    pub local: &'a dyn ModelClient,
    pub muse_spark: &'a dyn ModelClient,
    pub muse_wonder: &'a dyn ModelClient,
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
    };
    if client.destination() != dest {
        return Err(RouterError::BadResponse(format!(
            "client for {dest:?} declares {:?}; refusing",
            client.destination()
        )));
    }
    Ok((dest, client.complete(prompt)?))
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
        };
        assert!(matches!(
            dispatch(&b, Sensitivity::Sensitive, false, "PRIVATE"),
            Err(RouterError::PaidCallsDisabled)
        ));
        assert!(spark.seen.borrow().is_empty());
    }
}
