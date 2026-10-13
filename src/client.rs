//! The server's HTTP API, blocking, for the CLI (and the daemon to come).

use cce_sync::proto::{BatchOp, BatchRequest, Committed, Listing, Refusal, Status, H_DEVICE};
use ureq::Agent;

pub struct Client {
    agent: Agent,
    base: String,
    auth: String,
    device: String,
}

/// A batch's outcome: committed, or refused by the server's rules.
pub enum BatchResult {
    Committed(Committed),
    Refused(Refusal),
}

impl Client {
    pub fn new(server: &str, token: &str, device: &str) -> Client {
        let agent: Agent = Agent::config_builder().http_status_as_error(false).build().into();
        Client {
            agent,
            base: format!("{}/v1", server.trim_end_matches('/')),
            auth: format!("Bearer {token}"),
            device: device.to_string(),
        }
    }

    fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T, String> {
        let mut resp = self
            .agent
            .get(format!("{}/{path}", self.base))
            .header("authorization", &self.auth)
            .header(H_DEVICE, &self.device)
            .call()
            .map_err(|e| format!("GET {path}: {e}"))?;
        let status = resp.status().as_u16();
        if status != 200 {
            let body = resp.body_mut().read_to_string().unwrap_or_default();
            return Err(format!("GET {path}: HTTP {status}: {}", body.trim()));
        }
        resp.body_mut().read_json().map_err(|e| format!("GET {path}: {e}"))
    }

    pub fn listing(&self) -> Result<Listing, String> {
        self.get("files")
    }

    pub fn status(&self) -> Result<Status, String> {
        self.get("status")
    }

    pub fn batch(&self, ops: Vec<BatchOp>) -> Result<BatchResult, String> {
        let mut resp = self
            .agent
            .post(format!("{}/batch", self.base))
            .header("authorization", &self.auth)
            .header(H_DEVICE, &self.device)
            .send_json(BatchRequest { ops })
            .map_err(|e| format!("POST batch: {e}"))?;
        let status = resp.status().as_u16();
        let body = resp
            .body_mut()
            .with_config()
            .limit(64 * 1024 * 1024)
            .read_to_string()
            .map_err(|e| format!("POST batch: {e}"))?;
        match status {
            200 => serde_json::from_str(&body).map(BatchResult::Committed).map_err(|e| format!("POST batch: {e}")),
            400 | 404 | 409 | 413 => match serde_json::from_str(&body) {
                Ok(r) => Ok(BatchResult::Refused(r)),
                Err(_) => Err(format!("POST batch: HTTP {status}: {}", body.trim())),
            },
            _ => Err(format!("POST batch: HTTP {status}: {}", body.trim())),
        }
    }
}
