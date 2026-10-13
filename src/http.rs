//! The server's HTTP API, blocking, for the CLI and the daemon.

use cce_sync::proto::{
    encode_path, Applied, BatchOp, BatchRequest, ChangesPage, Committed, History, Listing, Pre, Refusal, Rev, Seq,
    Status, H_DEVICE, MAX_FILE_BYTES,
};
use ureq::Agent;

pub struct Client {
    agent: Agent,
    server: String,
    base: String,
    auth: String,
    device: String,
}

/// A write's outcome: done, or refused by the server's rules.
pub enum Answer<T> {
    Done(T),
    Refused(Refusal),
}

type Resp = ureq::http::Response<ureq::Body>;

impl Client {
    pub fn new(server: &str, token: &str, device: &str) -> Client {
        let agent: Agent = Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(std::time::Duration::from_secs(120)))
            .build()
            .into();
        let server = server.trim_end_matches('/').to_string();
        Client {
            agent,
            base: format!("{server}/v1"),
            server,
            auth: format!("Bearer {token}"),
            device: device.to_string(),
        }
    }

    pub fn server(&self) -> &str {
        &self.server
    }

    pub fn auth(&self) -> &str {
        &self.auth
    }

    /// The live socket's URL: `ws(s)://<server>/v1/live`.
    pub fn live_url(&self) -> String {
        let ws = if let Some(r) = self.base.strip_prefix("https://") {
            format!("wss://{r}")
        } else if let Some(r) = self.base.strip_prefix("http://") {
            format!("ws://{r}")
        } else {
            self.base.clone()
        };
        format!("{ws}/live")
    }

    fn read_body(resp: &mut Resp) -> Result<Vec<u8>, String> {
        resp.body_mut()
            .with_config()
            .limit(MAX_FILE_BYTES as u64 + 1024 * 1024)
            .read_to_vec()
            .map_err(|e| e.to_string())
    }

    /// GET a JSON document.
    fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T, String> {
        let bytes = self.get_bytes(path)?;
        serde_json::from_slice(&bytes).map_err(|e| format!("GET {path}: {e}"))
    }

    fn get_bytes(&self, path: &str) -> Result<Vec<u8>, String> {
        let mut resp = self
            .agent
            .get(format!("{}/{path}", self.base))
            .header("authorization", &self.auth)
            .header(H_DEVICE, &self.device)
            .call()
            .map_err(|e| format!("GET {path}: {e}"))?;
        let status = resp.status().as_u16();
        let body = Self::read_body(&mut resp).map_err(|e| format!("GET {path}: {e}"))?;
        if status != 200 {
            return Err(format!("GET {path}: HTTP {status}: {}", String::from_utf8_lossy(&body).trim()));
        }
        Ok(body)
    }

    /// A write's answer: 200 with `T`, or a refusal the caller acts on.
    fn answer<T: serde::de::DeserializeOwned>(what: &str, resp: Result<Resp, ureq::Error>) -> Result<Answer<T>, String> {
        let mut resp = resp.map_err(|e| format!("{what}: {e}"))?;
        let status = resp.status().as_u16();
        let body = Self::read_body(&mut resp).map_err(|e| format!("{what}: {e}"))?;
        match status {
            200 => serde_json::from_slice(&body).map(Answer::Done).map_err(|e| format!("{what}: {e}")),
            400 | 404 | 409 | 413 => match serde_json::from_slice(&body) {
                Ok(r) => Ok(Answer::Refused(r)),
                Err(_) => Err(format!("{what}: HTTP {status}: {}", String::from_utf8_lossy(&body).trim())),
            },
            _ => Err(format!("{what}: HTTP {status}: {}", String::from_utf8_lossy(&body).trim())),
        }
    }

    pub fn listing(&self) -> Result<Listing, String> {
        self.get("files")
    }

    pub fn status(&self) -> Result<Status, String> {
        self.get("status")
    }

    pub fn changes(&self, since: Seq) -> Result<ChangesPage, String> {
        self.get(&format!("changes?since={since}"))
    }

    pub fn history(&self, path: &str) -> Result<History, String> {
        self.get(&format!("history/{}", encode_path(path)))
    }

    /// The bytes with this hash (any version of any file).
    pub fn blob(&self, hash: &str) -> Result<Vec<u8>, String> {
        self.get_bytes(&format!("blobs/{hash}"))
    }

    pub fn put(&self, path: &str, pre: Pre, bytes: &[u8]) -> Result<Answer<Applied>, String> {
        let what = format!("PUT {path}");
        let req = self
            .agent
            .put(format!("{}/files/{}", self.base, encode_path(path)))
            .header("authorization", &self.auth)
            .header(H_DEVICE, &self.device);
        let req = match pre {
            Pre::Rev(r) => req.header("if-match", r.to_string()),
            Pre::Absent => req.header("if-none-match", "*"),
            Pre::Any => return Err(format!("{what}: a device write needs a precondition")),
        };
        match Self::answer::<Committed>(&what, req.send(bytes))? {
            Answer::Done(mut c) if !c.applied.is_empty() => Ok(Answer::Done(c.applied.remove(0))),
            Answer::Done(_) => Err(format!("{what}: empty answer")),
            Answer::Refused(r) => Ok(Answer::Refused(r)),
        }
    }

    pub fn delete(&self, path: &str, rev: Rev) -> Result<Answer<Committed>, String> {
        let what = format!("DELETE {path}");
        let resp = self
            .agent
            .delete(format!("{}/files/{}", self.base, encode_path(path)))
            .header("authorization", &self.auth)
            .header(H_DEVICE, &self.device)
            .header("if-match", rev.to_string())
            .call();
        Self::answer(&what, resp)
    }

    pub fn batch(&self, ops: Vec<BatchOp>) -> Result<Answer<Committed>, String> {
        let resp = self
            .agent
            .post(format!("{}/batch", self.base))
            .header("authorization", &self.auth)
            .header(H_DEVICE, &self.device)
            .send_json(BatchRequest { ops });
        Self::answer("POST batch", resp)
    }
}
