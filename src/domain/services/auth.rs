//! Native sign-in rules. No polling, browser callbacks or plaintext session file.

use crate::domain::models::native_auth::{AuthFailure, FinishedLogin, Handoff, Session};

pub trait Gateway {
    fn finish(&self, handoff: &Handoff) -> Result<FinishedLogin, AuthFailure>;
    fn cancel(&self, handoff: &Handoff) -> Result<(), AuthFailure>;
    fn logout(&self, session: &Session) -> Result<(), AuthFailure>;
}

pub trait SessionStore {
    fn read(&self) -> Result<Option<Session>, AuthFailure>;
    fn write(&self, session: &Session) -> Result<(), AuthFailure>;
    fn delete(&self) -> Result<(), AuthFailure>;
}

pub struct SignedIn {
    pub location_message: String,
    pub persisted: bool,
}

pub struct AuthService<G, S> {
    pub gateway: G,
    pub store: S,
    pub pending: Option<Handoff>,
    pub session: Option<Session>,
}

impl<G: Gateway, S: SessionStore> AuthService<G, S> {
    pub fn new(gateway: G, store: S) -> Self {
        Self {
            gateway,
            store,
            pending: None,
            session: None,
        }
    }

    pub fn start(&mut self) -> Result<&Handoff, AuthFailure> {
        self.pending = Some(Handoff::generate()?);
        Ok(self.pending.as_ref().unwrap())
    }

    pub fn finish(&mut self) -> Result<SignedIn, AuthFailure> {
        let pending = self.pending.as_ref().ok_or(AuthFailure::Expired)?;
        if pending.started.elapsed().as_secs() >= 1800 {
            self.pending = None;
            return Err(AuthFailure::Expired);
        }
        let result = match self.gateway.finish(pending) {
            Ok(value) if value.session.is_valid() => value,
            Ok(_) => return Err(AuthFailure::Invalid),
            Err(error) => {
                if !matches!(
                    error,
                    AuthFailure::NotFinished | AuthFailure::Transport | AuthFailure::RateLimited
                ) {
                    self.pending = None;
                }
                return Err(error);
            }
        };
        let persisted = self.store.write(&result.session).is_ok();
        let location_message = logged_in_message(result.city.as_deref(), result.country.as_deref());
        self.session = Some(result.session);
        self.pending = None;
        Ok(SignedIn {
            location_message,
            persisted,
        })
    }

    pub fn cancel(&mut self) -> Result<(), AuthFailure> {
        let pending = self.pending.as_ref().ok_or(AuthFailure::Expired)?;
        self.gateway.cancel(pending)?;
        self.pending = None;
        Ok(())
    }

    pub fn restore(&mut self) -> Result<bool, AuthFailure> {
        self.session = self.store.read()?;
        if self
            .session
            .as_ref()
            .is_some_and(|session| !session.is_valid())
        {
            self.session = None;
            self.store.delete()?;
            return Err(AuthFailure::Invalid);
        }
        Ok(self.session.is_some())
    }

    pub fn logout(&mut self) -> Result<(), AuthFailure> {
        if self.session.is_none() {
            self.restore()?;
        }
        if let Some(session) = self.session.as_ref() {
            // Keep credentials for a retry if the server does not confirm revocation.
            self.gateway.logout(session)?;
        }
        self.store.delete()?;
        self.session = None;
        Ok(())
    }
}

pub fn logged_in_message(city: Option<&str>, country: Option<&str>) -> String {
    fn clean(value: Option<&str>) -> Option<String> {
        value
            .map(str::trim)
            .filter(|s| !s.is_empty() && s.len() <= 200 && !s.chars().any(char::is_control))
            .map(str::to_owned)
    }
    match (clean(city), clean(country)) {
        (Some(city), Some(country)) => format!("Logged in from {city}, {country}"),
        (_, Some(country)) => format!("Logged in from {country}"),
        _ => "Logged in".to_owned(),
    }
}
