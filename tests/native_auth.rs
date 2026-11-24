use std::{
    cell::RefCell,
    collections::VecDeque,
    io::Cursor,
    time::{Duration, Instant},
};

use cortex_cli::{
    domain::{
        models::native_auth::{AuthFailure, FinishedLogin, Handoff, Session},
        services::auth::{AuthService, Gateway, SessionStore, logged_in_message},
    },
    presentation::cli::auth::finish_from_input,
};

fn session() -> Session {
    Session {
        session_id: "fixture-session".into(),
        csrf_token: "fixture-csrf".into(),
    }
}

fn finished() -> FinishedLogin {
    FinishedLogin {
        session: session(),
        city: Some("Bangkok".into()),
        country: Some("TH".into()),
    }
}

#[derive(Default)]
struct FakeGateway {
    responses: RefCell<VecDeque<Result<FinishedLogin, AuthFailure>>>,
    finishes: RefCell<usize>,
    cancellations: RefCell<usize>,
    fail: RefCell<Option<AuthFailure>>,
    logged_out: RefCell<Vec<String>>,
}

impl Gateway for FakeGateway {
    fn finish(&self, handoff: &Handoff) -> Result<FinishedLogin, AuthFailure> {
        assert_eq!(handoff.id.len(), 43);
        assert_eq!(handoff.secret.len(), 43);
        *self.finishes.borrow_mut() += 1;
        self.responses
            .borrow_mut()
            .pop_front()
            .expect("unexpected Finish request")
    }
    fn cancel(&self, _: &Handoff) -> Result<(), AuthFailure> {
        *self.cancellations.borrow_mut() += 1;
        self.fail.borrow().map_or(Ok(()), Err)
    }
    fn logout(&self, session: &Session) -> Result<(), AuthFailure> {
        if let Some(error) = *self.fail.borrow() {
            return Err(error);
        }
        self.logged_out
            .borrow_mut()
            .push(session.session_id.clone());
        Ok(())
    }
}

#[derive(Default)]
struct FakeStore {
    saved: RefCell<Option<String>>,
    unavailable: bool,
}

impl SessionStore for FakeStore {
    fn read(&self) -> Result<Option<Session>, AuthFailure> {
        if self.unavailable {
            return Err(AuthFailure::Keyring);
        }
        Ok(self
            .saved
            .borrow()
            .as_ref()
            .map(|s| serde_json::from_str(s).unwrap()))
    }
    fn write(&self, value: &Session) -> Result<(), AuthFailure> {
        if self.unavailable {
            return Err(AuthFailure::Keyring);
        }
        *self.saved.borrow_mut() = Some(serde_json::to_string(value).unwrap());
        Ok(())
    }
    fn delete(&self) -> Result<(), AuthFailure> {
        if self.unavailable {
            return Err(AuthFailure::Keyring);
        }
        self.saved.borrow_mut().take();
        Ok(())
    }
}

#[test]
fn challenge_hashes_raw_secret_and_match_code_uses_base32_digest() {
    let handoff = Handoff::from_bytes([42; 32], std::array::from_fn(|i| i as u8));
    assert_eq!(
        handoff.challenge,
        "Yw3NKWbEM2aRElRIu7JbT_QSpJxzLbLIq8G4WBvXEN0"
    );
    assert_eq!(handoff.match_code, "MMG4");
    let second = Handoff::generate().unwrap();
    let third = Handoff::generate().unwrap();
    assert_ne!(second.id, third.id);
    assert_ne!(second.secret, third.secret);
}

#[test]
fn finish_only_runs_after_enter_and_preserves_pending_for_not_finished() {
    let gateway = FakeGateway::default();
    gateway
        .responses
        .borrow_mut()
        .extend([Err(AuthFailure::NotFinished), Ok(finished())]);
    let mut service = AuthService::new(gateway, FakeStore::default());
    service.start().unwrap();
    let id = service.pending.as_ref().unwrap().id.clone();
    let secret = service.pending.as_ref().unwrap().secret.clone();
    let mut output = Vec::new();
    finish_from_input(&mut service, &mut Cursor::new("ignored\n\n\n"), &mut output).unwrap();
    assert_eq!(*service.gateway.finishes.borrow(), 2);
    assert!(service.pending.is_none());
    assert!(service.session.is_some());
    assert!(service.store.read().unwrap().is_some());
    let text = String::from_utf8(output).unwrap();
    assert!(text.contains("The login is not finished"));
    assert!(text.contains("Logged in from Bangkok, TH"));
    for hidden in [id, secret, "fixture-session".into(), "fixture-csrf".into()] {
        assert!(!text.contains(&hidden));
    }
}

#[test]
fn no_keyring_keeps_only_process_session_and_requires_sign_in_again() {
    let gateway = FakeGateway::default();
    gateway.responses.borrow_mut().push_back(Ok(finished()));
    let mut service = AuthService::new(
        gateway,
        FakeStore {
            unavailable: true,
            ..Default::default()
        },
    );
    service.start().unwrap();
    let mut output = Vec::new();
    finish_from_input(&mut service, &mut Cursor::new("\n"), &mut output).unwrap();
    assert!(service.session.is_some());
    assert!(service.store.saved.borrow().is_none());
    assert!(
        String::from_utf8(output)
            .unwrap()
            .contains("Sign in again for each session")
    );
}

#[test]
fn terminal_errors_end_handoff_and_do_not_store_session() {
    for error in [
        AuthFailure::Expired,
        AuthFailure::Used,
        AuthFailure::Cancelled,
        AuthFailure::Refused,
        AuthFailure::Invalid,
        AuthFailure::Disabled,
    ] {
        let gateway = FakeGateway::default();
        gateway.responses.borrow_mut().push_back(Err(error));
        let mut service = AuthService::new(gateway, FakeStore::default());
        service.start().unwrap();
        assert!(matches!(service.finish(), Err(e) if e == error));
        assert!(service.pending.is_none());
        assert!(service.session.is_none());
        assert!(service.store.read().unwrap().is_none());
    }
}

#[test]
fn local_expiry_makes_no_finish_request() {
    let mut service = AuthService::new(FakeGateway::default(), FakeStore::default());
    service.start().unwrap();
    service.pending.as_mut().unwrap().started = Instant::now() - Duration::from_secs(1801);
    assert!(matches!(service.finish(), Err(AuthFailure::Expired)));
    assert_eq!(*service.gateway.finishes.borrow(), 0);
}

#[test]
fn invalid_success_credentials_end_handoff_without_persistence() {
    let gateway = FakeGateway::default();
    let mut invalid = finished();
    invalid.session.csrf_token.clear();
    gateway.responses.borrow_mut().push_back(Ok(invalid));
    let mut service = AuthService::new(gateway, FakeStore::default());
    service.start().unwrap();
    assert!(matches!(service.finish(), Err(AuthFailure::Invalid)));
    assert!(service.pending.is_none());
    assert!(service.session.is_none());
    assert!(service.store.read().unwrap().is_none());
}

#[test]
fn cancellation_and_eof_each_send_one_cancel_and_no_finish() {
    for input in ["cancel\n", ""] {
        let mut service = AuthService::new(FakeGateway::default(), FakeStore::default());
        service.start().unwrap();
        finish_from_input(&mut service, &mut Cursor::new(input), &mut Vec::new()).unwrap();
        assert_eq!(*service.gateway.cancellations.borrow(), 1);
        assert_eq!(*service.gateway.finishes.borrow(), 0);
        assert!(service.pending.is_none());
    }
}

#[test]
fn failed_cancellation_and_logout_keep_credentials_for_explicit_retry() {
    let mut service = AuthService::new(FakeGateway::default(), FakeStore::default());
    service.start().unwrap();
    *service.gateway.fail.borrow_mut() = Some(AuthFailure::Transport);
    assert!(service.cancel().is_err());
    assert!(service.pending.is_some());
    service.store.write(&session()).unwrap();
    assert!(service.logout().is_err());
    assert!(service.store.read().unwrap().is_some());
    *service.gateway.fail.borrow_mut() = None;
    service.logout().unwrap();
    assert!(service.session.is_none());
    assert!(service.store.read().unwrap().is_none());
    assert_eq!(*service.gateway.logged_out.borrow(), ["fixture-session"]);
}

#[test]
fn location_fallbacks_and_terminal_escape_filter_are_explicit() {
    assert_eq!(logged_in_message(None, Some("BR")), "Logged in from BR");
    assert_eq!(logged_in_message(None, None), "Logged in");
    assert_eq!(logged_in_message(Some("city"), None), "Logged in");
    assert_eq!(
        logged_in_message(Some("\x1b[31m"), Some("TH")),
        "Logged in from TH"
    );
}
