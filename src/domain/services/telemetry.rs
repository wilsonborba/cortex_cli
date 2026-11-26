use crate::domain::models::{
    native_auth::Session,
    telemetry::{LegalStatus, TelemetryConsentRecord},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TelemetryError;

pub trait LegalTelemetryApi {
    fn legal_status(&self, session: &Session) -> Result<LegalStatus, TelemetryError>;
}

pub trait ConsentStore {
    fn read(&self) -> Result<Option<TelemetryConsentRecord>, TelemetryError>;
    fn write(&self, record: &TelemetryConsentRecord) -> Result<(), TelemetryError>;
}

pub trait TrackingApi: Send + Sync {
    fn post_batch(
        &self,
        session: &Session,
        batch: &crate::domain::models::telemetry::TrackingBatch,
    );
    fn post_error(
        &self,
        session: &Session,
        error: &crate::domain::models::telemetry::TrackingError,
    );
}

pub struct Telemetry {
    api: std::sync::Arc<dyn TrackingApi>,
    session: std::sync::Arc<Session>,
    tracking_session_id: String,
    next_seq: std::sync::atomic::AtomicU64,
}

impl Telemetry {
    pub fn new(
        api: std::sync::Arc<dyn TrackingApi>,
        session: std::sync::Arc<Session>,
    ) -> Option<Self> {
        Some(Self {
            api,
            session,
            tracking_session_id: tracking_uuid()?,
            next_seq: std::sync::atomic::AtomicU64::new(0),
        })
    }

    pub fn chat_result(&self, result: Result<(), crate::domain::models::chat::ChatFailure>) {
        use crate::domain::models::telemetry::{TrackingBatch, TrackingError, TrackingEvent};
        let (event_type, element_id, element_label) = match result {
            Ok(()) => ("submit", "chat.request.completed", "Chat request completed"),
            Err(_) => ("error_link", "chat.request.failed", "Chat request failed"),
        };
        self.api.post_batch(
            &self.session,
            &TrackingBatch {
                app_id: "cortex",
                session_id: self.tracking_session_id.clone(),
                events: vec![TrackingEvent {
                    seq: self
                        .next_seq
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed),
                    event_type,
                    screen: "terminal_chat",
                    route: "/chat",
                    path: "/chat",
                    element_id,
                    element_label,
                    element_path: "terminal > chat > request",
                    value: None,
                    sensitive: false,
                }],
            },
        );
        if let Err(error) = result {
            let (error_type, message) = match error {
                crate::domain::models::chat::ChatFailure::Connection => {
                    ("connection", "Chat connection failed.")
                }
                crate::domain::models::chat::ChatFailure::Unauthorized => {
                    ("unauthorized", "Chat request was not authorized.")
                }
                crate::domain::models::chat::ChatFailure::InvalidResponse => {
                    ("invalid_response", "Chat response was invalid.")
                }
                crate::domain::models::chat::ChatFailure::IncompleteStream => (
                    "incomplete_stream",
                    "Chat response ended before completion.",
                ),
                crate::domain::models::chat::ChatFailure::Rejected(_) => {
                    ("rejected", "Chat request was rejected.")
                }
                crate::domain::models::chat::ChatFailure::InvalidInput => {
                    ("invalid_input", "Chat request input was invalid.")
                }
                crate::domain::models::chat::ChatFailure::SourceUnavailable => (
                    "source_unavailable",
                    "Selected source context is unavailable.",
                ),
                crate::domain::models::chat::ChatFailure::RunningOutput => {
                    ("running_output", "Chat already had a running output.")
                }
            };
            self.api.post_error(
                &self.session,
                &TrackingError {
                    app_id: "cortex",
                    session_id: self.tracking_session_id.clone(),
                    route: "/chat",
                    error_type,
                    message,
                    build_version: crate::core::settings::VERSION,
                },
            );
        }
    }
}

fn tracking_uuid() -> Option<String> {
    use rand::{TryRngCore, rngs::OsRng};
    let mut bytes = [0u8; 16];
    OsRng.try_fill_bytes(&mut bytes).ok()?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Some(format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0],
        bytes[1],
        bytes[2],
        bytes[3],
        bytes[4],
        bytes[5],
        bytes[6],
        bytes[7],
        bytes[8],
        bytes[9],
        bytes[10],
        bytes[11],
        bytes[12],
        bytes[13],
        bytes[14],
        bytes[15]
    ))
}

pub fn telemetry_is_enabled(
    status: &LegalStatus,
    record: Option<&TelemetryConsentRecord>,
    session_fingerprint: &str,
) -> bool {
    let policy = &status.privacy_policy;
    policy.accepted_current_version()
        && record.is_some_and(|record| {
            record.accepted
                && record.session_fingerprint == session_fingerprint
                && record.privacy_version == policy.current_version
        })
}

pub fn establish_consent(
    api: &impl LegalTelemetryApi,
    store: &impl ConsentStore,
    session: &Session,
    session_fingerprint: &str,
    interactive: bool,
    mut ask: impl FnMut(&str) -> Option<bool>,
) -> Option<TelemetryConsentRecord> {
    let status = api.legal_status(session).ok()?;
    if status.privacy_policy.current_version.is_empty()
        || status.privacy_policy.current_version.len() > 40
    {
        return None;
    }
    let current_version = status.privacy_policy.current_version.as_str();
    let saved = store.read().ok().flatten();
    if telemetry_is_enabled(&status, saved.as_ref(), session_fingerprint) {
        return saved;
    }
    if saved.as_ref().is_some_and(|record| {
        record.session_fingerprint == session_fingerprint
            && record.privacy_version == current_version
            && !record.accepted
    }) || !interactive
    {
        return None;
    }
    // Telemetry opt-in is its own explicit choice. Never turn it into
    // acceptance of the globally shared privacy policy.
    if !status.privacy_policy.accepted_current_version() {
        return None;
    }
    let accepted = ask(current_version)?;
    let record = TelemetryConsentRecord {
        session_fingerprint: session_fingerprint.to_owned(),
        privacy_version: current_version.to_owned(),
        accepted,
    };
    store.write(&record).ok()?;
    accepted.then_some(record)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::telemetry::LegalDocumentStatus;

    struct FakeLegalApi(LegalStatus);

    impl LegalTelemetryApi for FakeLegalApi {
        fn legal_status(&self, _: &Session) -> Result<LegalStatus, TelemetryError> {
            Ok(self.0.clone())
        }
    }

    #[derive(Default)]
    struct FakeConsentStore(std::cell::RefCell<Option<TelemetryConsentRecord>>);

    impl ConsentStore for FakeConsentStore {
        fn read(&self) -> Result<Option<TelemetryConsentRecord>, TelemetryError> {
            Ok(self.0.borrow().clone())
        }

        fn write(&self, record: &TelemetryConsentRecord) -> Result<(), TelemetryError> {
            self.0.replace(Some(record.clone()));
            Ok(())
        }
    }

    fn status(accepted: bool, version: &str, current: &str) -> LegalStatus {
        LegalStatus {
            privacy_policy: LegalDocumentStatus {
                accepted,
                version: Some(version.into()),
                current_version: current.into(),
            },
        }
    }

    fn record(account: &str, version: &str, accepted: bool) -> TelemetryConsentRecord {
        TelemetryConsentRecord {
            session_fingerprint: account.into(),
            privacy_version: version.into(),
            accepted,
        }
    }

    fn session() -> Session {
        Session {
            session_id: "session-1".into(),
            csrf_token: "csrf-1".into(),
        }
    }

    #[test]
    fn missing_current_privacy_acceptance_never_prompts_or_records_telemetry_consent() {
        let api = FakeLegalApi(status(false, "2026-09-13", "2026-09-14"));
        let store = FakeConsentStore::default();
        let prompted = std::cell::Cell::new(false);

        assert!(
            establish_consent(&api, &store, &session(), "account-a", true, |_| {
                prompted.set(true);
                Some(true)
            })
            .is_none()
        );
        assert!(!prompted.get());
        assert!(store.0.borrow().is_none());
    }

    #[test]
    fn affirmative_telemetry_choice_records_only_local_opt_in() {
        let api = FakeLegalApi(status(true, "2026-09-14", "2026-09-14"));
        let store = FakeConsentStore::default();

        let consent =
            establish_consent(&api, &store, &session(), "account-a", true, |_| Some(true))
                .expect("explicit telemetry opt-in");
        assert_eq!(consent, record("account-a", "2026-09-14", true));
        assert_eq!(*store.0.borrow(), Some(consent));
    }

    #[test]
    fn decline_is_saved_and_never_enables_telemetry() {
        let api = FakeLegalApi(status(true, "2026-09-14", "2026-09-14"));
        let store = FakeConsentStore::default();

        assert!(
            establish_consent(&api, &store, &session(), "account-a", true, |_| Some(false),)
                .is_none()
        );
        assert_eq!(
            *store.0.borrow(),
            Some(record("account-a", "2026-09-14", false))
        );
    }

    #[test]
    fn telemetry_requires_signed_in_current_policy_and_explicit_current_account_choice() {
        assert!(!telemetry_is_enabled(
            &status(false, "2026-09-14", "2026-09-14"),
            Some(&record("session-a", "2026-09-14", true)),
            "session-a"
        ));
        assert!(!telemetry_is_enabled(
            &status(true, "2026-09-14", "2026-09-14"),
            None,
            "session-a"
        ));
        assert!(!telemetry_is_enabled(
            &status(true, "2026-09-14", "2026-09-14"),
            Some(&record("session-a", "2026-09-14", false)),
            "session-a"
        ));
        assert!(!telemetry_is_enabled(
            &status(true, "2026-09-14", "2026-09-14"),
            Some(&record("session-b", "2026-09-14", true)),
            "session-a"
        ));
        assert!(!telemetry_is_enabled(
            &status(true, "2026-09-14", "2026-10-01"),
            Some(&record("session-a", "2026-09-14", true)),
            "session-a"
        ));
        assert!(telemetry_is_enabled(
            &status(true, "2026-09-14", "2026-09-14"),
            Some(&record("session-a", "2026-09-14", true)),
            "session-a"
        ));
    }
}
