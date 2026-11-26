//! Data access boundary. Domain services use adapters from this layer.
//!
//! Gateway, Cortex API and external provider transports belong in `remote`.
//! Keyring and local persistence adapters belong in `local`.

pub mod local {
    pub mod provider_key;
    pub mod session;
    pub mod telemetry_consent;
    pub mod voice;
}
pub mod remote {
    pub mod chat;
    pub mod gateway;
    pub mod login_context;
    pub mod provider;
    pub mod remote_sse;
    pub mod sse;
}
