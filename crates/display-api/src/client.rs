//! Blocking JSON-RPC client.
//!
//! Blocking on purpose: the CLI has no use for an async runtime, and the GUI
//! speaks this protocol from Swift rather than through this crate.

use crate::protocol::*;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("daemon not running (socket {0})")]
    NotRunning(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("protocol: {0}")]
    Protocol(String),
    #[error("daemon error {code}: {message}")]
    Rpc { code: i32, message: String },
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

pub struct Client {
    stream: UnixStream,
    reader: BufReader<UnixStream>,
    next_id: u64,
}

impl Client {
    pub fn connect(path: &Path) -> Result<Self, ClientError> {
        let stream = UnixStream::connect(path).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused => {
                ClientError::NotRunning(path.display().to_string())
            }
            _ => ClientError::Io(e),
        })?;
        // A wedged I2C transaction must not hang the CLI forever. Generous
        // enough for a slow capability read with retries.
        stream.set_read_timeout(Some(Duration::from_secs(30)))?;
        let reader = BufReader::new(stream.try_clone()?);
        Ok(Client {
            stream,
            reader,
            next_id: 1,
        })
    }

    pub fn call<T: serde::de::DeserializeOwned>(
        &mut self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<T, ClientError> {
        let id = self.next_id;
        self.next_id += 1;

        let req = Request::new(id, method, params);
        let mut line = serde_json::to_string(&req)?;
        line.push('\n');
        self.stream.write_all(line.as_bytes())?;
        self.stream.flush()?;

        let mut buf = String::new();
        if self.reader.read_line(&mut buf)? == 0 {
            return Err(ClientError::Protocol("daemon closed connection".into()));
        }
        let resp: Response = serde_json::from_str(buf.trim())?;
        if resp.id != id {
            return Err(ClientError::Protocol(format!(
                "response id {} does not match request id {id}",
                resp.id
            )));
        }
        if let Some(e) = resp.error {
            return Err(ClientError::Rpc {
                code: e.code,
                message: e.message,
            });
        }
        let result = resp
            .result
            .ok_or_else(|| ClientError::Protocol("response has neither result nor error".into()))?;
        Ok(serde_json::from_value(result)?)
    }

    pub fn version(&mut self) -> Result<VersionInfo, ClientError> {
        self.call(method::VERSION, None)
    }

    pub fn list(&mut self) -> Result<Vec<MonitorInfo>, ClientError> {
        self.call(method::LIST, None)
    }

    pub fn get(&mut self, display: &str, code: &str) -> Result<VcpValue, ClientError> {
        self.call(
            method::GET,
            Some(serde_json::json!({ "display": display, "code": code })),
        )
    }

    pub fn set(
        &mut self,
        display: &str,
        code: &str,
        value: u16,
        verify: bool,
    ) -> Result<SetResult, ClientError> {
        self.call(
            method::SET,
            Some(serde_json::json!({
                "display": display, "code": code, "value": value, "verify": verify
            })),
        )
    }

    pub fn caps(&mut self, display: &str) -> Result<CapsResult, ClientError> {
        self.call(
            method::CAPS,
            Some(serde_json::json!({ "display": display })),
        )
    }

    pub fn profile_list(&mut self) -> Result<Vec<ProfileSummary>, ClientError> {
        self.call(method::PROFILE_LIST, None)
    }

    pub fn profile_apply(
        &mut self,
        name: &str,
        verify: bool,
        yes: bool,
    ) -> Result<ApplyResult, ClientError> {
        self.call(
            method::PROFILE_APPLY,
            Some(serde_json::json!({ "name": name, "verify": verify, "yes": yes })),
        )
    }

    pub fn profile_save(
        &mut self,
        name: &str,
        codes: &[String],
        force: bool,
    ) -> Result<ProfileSummary, ClientError> {
        self.call(
            method::PROFILE_SAVE,
            Some(serde_json::json!({ "name": name, "codes": codes, "force": force })),
        )
    }

    pub fn profile_delete(&mut self, name: &str) -> Result<(), ClientError> {
        let _: serde_json::Value = self.call(
            method::PROFILE_DELETE,
            Some(serde_json::json!({ "name": name })),
        )?;
        Ok(())
    }

    pub fn profile_show(&mut self, name: &str) -> Result<serde_json::Value, ClientError> {
        self.call(
            method::PROFILE_SHOW,
            Some(serde_json::json!({ "name": name })),
        )
    }

    pub fn rules_list(&mut self) -> Result<Vec<RuleInfo>, ClientError> {
        self.call(method::RULES_LIST, None)
    }

    pub fn rules_reload(&mut self) -> Result<Vec<RuleInfo>, ClientError> {
        self.call(method::RULES_RELOAD, None)
    }

    pub fn rules_tick(&mut self) -> Result<Vec<RuleFired>, ClientError> {
        self.call(method::RULES_TICK, None)
    }
}
