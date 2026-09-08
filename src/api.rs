//! Victual's worker API. Nine routes, and no destination that is not derived from one base URL.

use serde_json::Value;

pub struct Api {
    base: String,
    key: std::sync::Mutex<String>,
    agent: ureq::Agent,
}

#[derive(Debug)]
pub enum ApiError {
    /// The credential was refused. Routine across a rotation, and never a reason to discard
    /// an outcome the worker is holding: ADR-0019 is explicit that a 401 on `result` means
    /// retry after rotating, not that what the device did is lost.
    Unauthorized(String),
    Other(String),
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::Unauthorized(m) | ApiError::Other(m) => write!(f, "{m}"),
        }
    }
}

impl Api {
    pub fn new(base: &str, key: &str, timeout_seconds: u64) -> Self {
        Api {
            base: base.trim_end_matches('/').to_string(),
            key: std::sync::Mutex::new(key.to_string()),
            agent: ureq::AgentBuilder::new()
                .timeout(std::time::Duration::from_secs(timeout_seconds))
                .build(),
        }
    }

    /// The base URL, so a heartbeat thread can build its own client rather than share this
    /// one's connection pool across threads.
    pub fn base_url(&self) -> String {
        self.base.clone()
    }

    pub fn credential(&self) -> String {
        self.key.lock().unwrap().clone()
    }

    pub fn register(&self, definitions: Value) -> Result<Value, ApiError> {
        self.post("/api/labels/register", definitions)
    }

    pub fn claim(&self, limit: u32) -> Result<Vec<Value>, ApiError> {
        let body = self.post("/api/labels/jobs/claim", serde_json::json!({"limit": limit}))?;
        Ok(body.as_array().cloned().unwrap_or_default())
    }

    pub fn artifact_bytes(&self, artifact_id: u64) -> Result<Vec<u8>, ApiError> {
        let url = format!("{}/api/labels/artifacts/{}/bytes", self.base, artifact_id);
        let key = self.key.lock().unwrap().clone();
        let response = self.agent.get(&url).set("VICTUAL-API-KEY", &key).call()
            .map_err(|e| Self::classify(&url, e))?;
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut response.into_reader(), &mut bytes)
            .map_err(|e| ApiError::Other(format!("reading {url}: {e}")))?;
        Ok(bytes)
    }

    pub fn heartbeat(&self, attempt: u64) -> Result<Value, ApiError> {
        self.post(&format!("/api/labels/attempts/{attempt}/heartbeat"), serde_json::json!({}))
    }

    pub fn sent(&self, attempt: u64, bytes: usize) -> Result<Value, ApiError> {
        self.post(&format!("/api/labels/attempts/{attempt}/sent"), serde_json::json!({"byte_length": bytes}))
    }

    pub fn result(&self, attempt: u64, outcome: &str, detail: Value) -> Result<Value, ApiError> {
        self.post(&format!("/api/labels/attempts/{attempt}/result"),
            serde_json::json!({"outcome": outcome, "detail": detail}))
    }

    pub fn status(&self, printer: u64, report: Value) -> Result<Value, ApiError> {
        self.post(&format!("/api/labels/printers/{printer}/status"), report)
    }

    fn post(&self, path: &str, body: Value) -> Result<Value, ApiError> {
        let url = format!("{}{path}", self.base);
        let key = self.key.lock().unwrap().clone();
        match self.agent.post(&url).set("VICTUAL-API-KEY", &key).send_json(body) {
            Ok(ok) => ok.into_json().map_err(|e| ApiError::Other(format!("decoding {url}: {e}"))),
            Err(e) => Err(Self::classify(&url, e)),
        }
    }

    fn classify(url: &str, error: ureq::Error) -> ApiError {
        match error {
            ureq::Error::Status(401, _) => ApiError::Unauthorized(format!("{url}: HTTP 401")),
            ureq::Error::Status(code, body) => {
                ApiError::Other(format!("{url}: HTTP {code}: {}", body.into_string().unwrap_or_default()))
            }
            other => ApiError::Other(format!("{url}: {other}")),
        }
    }
}
