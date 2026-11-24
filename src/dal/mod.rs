//! Data access boundary. Domain services use adapters from this layer.
//!
//! Gateway, Cortex API and external provider transports belong in `remote`.
//! Keyring and local persistence adapters belong in `local`.

pub mod local {}
pub mod remote {}
