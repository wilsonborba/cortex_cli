//! Encode the browser's existing cosmetic app context. This is not authentication.

use fernet::Fernet;
use serde_json::json;

use crate::{
    core::settings,
    domain::models::native_auth::{AuthFailure, Handoff},
};

pub fn browser_url(handoff: &Handoff) -> Result<String, AuthFailure> {
    let context = json!({"name":"cortex", "platform":"cli", "handoff_id":handoff.id, "challenge":handoff.challenge});
    let fernet = Fernet::new(settings::APP_CONTEXT_KEY).ok_or(AuthFailure::Invalid)?;
    let token = fernet.encrypt(context.to_string().as_bytes());
    let mut url = reqwest::Url::parse(settings::LOGIN_URL).map_err(|_| AuthFailure::Invalid)?;
    url.query_pairs_mut().append_pair("app_context", &token);
    Ok(url.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_receives_only_cosmetic_context_never_the_secret() {
        let handoff = Handoff::from_bytes([42; 32], [24; 32]);
        let link = browser_url(&handoff).unwrap();
        assert!(!link.contains(&handoff.id));
        assert!(!link.contains(&handoff.secret));
        let url = reqwest::Url::parse(&link).unwrap();
        assert_eq!(
            url.origin().ascii_serialization(),
            "https://auth.asodya.com"
        );
        assert_eq!(url.path(), "/login");
        let pairs: Vec<_> = url.query_pairs().collect();
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].0, "app_context");
        let decrypted = Fernet::new(settings::APP_CONTEXT_KEY)
            .unwrap()
            .decrypt(&pairs[0].1)
            .unwrap();
        let context: serde_json::Value = serde_json::from_slice(&decrypted).unwrap();
        assert_eq!(
            context,
            json!({"name":"cortex", "platform":"cli", "handoff_id":handoff.id, "challenge":handoff.challenge})
        );
        assert!(
            !String::from_utf8(decrypted)
                .unwrap()
                .contains(&handoff.secret)
        );
    }
}
