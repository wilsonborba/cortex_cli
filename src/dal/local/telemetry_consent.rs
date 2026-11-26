use crate::domain::{
    models::telemetry::TelemetryConsentRecord,
    services::telemetry::{ConsentStore, TelemetryError},
};

pub struct OsTelemetryConsentStore;

impl OsTelemetryConsentStore {
    fn entry(&self) -> Result<keyring::Entry, TelemetryError> {
        keyring::Entry::new("com.asodya.cortex.cli", "asodya-telemetry-consent")
            .map_err(|_| TelemetryError)
    }
}

impl ConsentStore for OsTelemetryConsentStore {
    fn read(&self) -> Result<Option<TelemetryConsentRecord>, TelemetryError> {
        match self.entry()?.get_password() {
            Ok(value) => serde_json::from_str(&value)
                .map(Some)
                .map_err(|_| TelemetryError),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(_) => Err(TelemetryError),
        }
    }

    fn write(&self, record: &TelemetryConsentRecord) -> Result<(), TelemetryError> {
        let value = serde_json::to_string(record).map_err(|_| TelemetryError)?;
        self.entry()?
            .set_password(&value)
            .map_err(|_| TelemetryError)
    }
}
