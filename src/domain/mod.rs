//! Business data and services. No terminal or transport behavior belongs here.

pub mod models {
    pub mod chat;
    pub mod native_auth;
    pub mod provider;
}
pub mod services {
    pub mod auth;
    pub mod chat;
}
