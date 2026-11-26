use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct LegalDocumentStatus {
    pub accepted: bool,
    pub version: Option<String>,
    pub current_version: String,
}

impl LegalDocumentStatus {
    pub fn accepted_current_version(&self) -> bool {
        self.accepted
            && self.version.as_deref() == Some(self.current_version.as_str())
            && !self.current_version.is_empty()
            && self.current_version.len() <= 40
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct LegalStatus {
    pub privacy_policy: LegalDocumentStatus,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TelemetryConsentRecord {
    pub session_fingerprint: String,
    pub privacy_version: String,
    pub accepted: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TrackingBatch {
    pub app_id: &'static str,
    pub session_id: String,
    pub events: Vec<TrackingEvent>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TrackingEvent {
    pub seq: u64,
    pub event_type: &'static str,
    pub screen: &'static str,
    pub route: &'static str,
    pub path: &'static str,
    pub element_id: &'static str,
    pub element_label: &'static str,
    pub element_path: &'static str,
    pub value: Option<String>,
    pub sensitive: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TrackingError {
    pub app_id: &'static str,
    pub session_id: String,
    pub route: &'static str,
    pub error_type: &'static str,
    pub message: &'static str,
    pub build_version: &'static str,
}
