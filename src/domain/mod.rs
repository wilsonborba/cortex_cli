//! Business data and services. No terminal or transport behavior belongs here.

pub mod models {
    pub mod annotations;
    pub mod chat;
    pub mod native_auth;
}
pub mod services {
    pub mod annotations;
    pub mod auth;
    pub mod chat;
}
