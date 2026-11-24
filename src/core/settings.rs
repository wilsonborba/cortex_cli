//! Non-secret application metadata. Credentials never belong here.

pub const APP_NAME: &str = "Cortex";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

// Public app-context presentation key already shipped in the auth frontend.
// The gateway verifies the separate random handoff secret.
pub(crate) const APP_CONTEXT_KEY: &str = "mEt5jdm9aTbUYnjhQM_tY_CTQL-JvXe0u9VdKEM2KmY=";
pub const GATEWAY_URL: &str = "https://api.asodya.com";
pub const CORTEX_API_PATH: &str = "/apps/cortex/v1";
pub const LOGIN_URL: &str = "https://auth.asodya.com/login";
