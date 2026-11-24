//! Native credential store. Never falls back to a file or environment variable.

use zeroize::Zeroizing;

use crate::domain::{
    models::native_auth::{AuthFailure, Session},
    services::auth::SessionStore,
};

pub struct OsSessionStore;

impl OsSessionStore {
    fn entry(&self) -> Result<keyring::Entry, AuthFailure> {
        keyring::Entry::new("com.asodya.cortex.cli", "asodya-session")
            .map_err(|_| AuthFailure::Keyring)
    }
}

impl SessionStore for OsSessionStore {
    fn read(&self) -> Result<Option<Session>, AuthFailure> {
        match self.entry()?.get_password() {
            Ok(value) => serde_json::from_str(&Zeroizing::new(value))
                .map(Some)
                .map_err(|_| AuthFailure::Invalid),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(_) => Err(AuthFailure::Keyring),
        }
    }

    fn write(&self, session: &Session) -> Result<(), AuthFailure> {
        let value =
            Zeroizing::new(serde_json::to_string(session).map_err(|_| AuthFailure::Invalid)?);
        self.entry()?
            .set_password(&value)
            .map_err(|_| AuthFailure::Keyring)
    }

    fn delete(&self) -> Result<(), AuthFailure> {
        match self.entry()?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(_) => Err(AuthFailure::Keyring),
        }
    }
}
