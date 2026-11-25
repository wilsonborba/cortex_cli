//! Provider API keys use separate OS keyring entries from the Asodya session.

use zeroize::Zeroizing;

use crate::domain::models::provider::Provider;

const SERVICE: &str = "com.asodya.cortex.cli.providers";

fn entry(provider: Provider) -> Result<keyring::Entry, ProviderKeyError> {
    keyring::Entry::new(SERVICE, provider.id()).map_err(|_| ProviderKeyError)
}

#[derive(Clone, Copy, Debug)]
pub struct ProviderKeyError;

impl std::fmt::Display for ProviderKeyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("OS keyring operation failed")
    }
}

impl std::error::Error for ProviderKeyError {}

pub fn read(provider: Provider) -> Result<Option<Zeroizing<String>>, ProviderKeyError> {
    match entry(provider)?.get_password() {
        Ok(value) => Ok(Some(Zeroizing::new(value))),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(_) => Err(ProviderKeyError),
    }
}

pub fn write(provider: Provider, key: &str) -> Result<(), ProviderKeyError> {
    entry(provider)?
        .set_password(key)
        .map_err(|_| ProviderKeyError)
}

pub fn delete(provider: Provider) -> Result<(), ProviderKeyError> {
    match entry(provider)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(_) => Err(ProviderKeyError),
    }
}

pub fn configured(provider: Provider) -> Result<bool, ProviderKeyError> {
    Ok(read(provider)?.is_some())
}

#[cfg(test)]
mod tests {
    use super::SERVICE;

    #[test]
    fn provider_keys_use_a_service_separate_from_the_asodya_session() {
        assert_eq!(SERVICE, "com.asodya.cortex.cli.providers");
        assert_ne!(SERVICE, "com.asodya.cortex.cli");
    }
}
