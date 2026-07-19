//! Wire types.
//!
//! The protocol is versioned from the first commit: [`PROTOCOL_VERSION`] is
//! reported by `protocol.version` so a client can refuse to drive a daemon it
//! does not understand, rather than misinterpreting fields.

use serde::{Deserialize, Serialize};

/// Bumped on any breaking change to the shapes below.
pub const PROTOCOL_VERSION: u32 = 1;

// ── JSON-RPC envelope ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub jsonrpc: String,
    pub id: u64,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

impl Request {
    pub fn new(id: u64, method: &str, params: Option<serde_json::Value>) -> Self {
        Request {
            jsonrpc: "2.0".into(),
            id,
            method: method.into(),
            params,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub jsonrpc: String,
    pub id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

impl Response {
    pub fn ok(id: u64, result: serde_json::Value) -> Self {
        Response {
            jsonrpc: "2.0".into(),
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn err(id: u64, code: i32, message: impl Into<String>) -> Self {
        Response {
            jsonrpc: "2.0".into(),
            id,
            result: None,
            error: Some(RpcError {
                code,
                message: message.into(),
            }),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
}

// Standard JSON-RPC codes.
pub const METHOD_NOT_FOUND: i32 = -32601;
pub const INVALID_PARAMS: i32 = -32602;
pub const INTERNAL_ERROR: i32 = -32603;

// ── Domain types ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ControlPath {
    Ddc,
    Native,
    None,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MonitorInfo {
    pub id: u32,
    pub vendor: String,
    pub product: String,
    pub serial: i64,
    pub alphanumeric_serial: String,
    /// False when `serial` is a placeholder, so a UI can avoid showing it as
    /// though it identified the unit.
    pub serial_trustworthy: bool,
    pub location: String,
    pub control: ControlPath,
    /// Stable key used to persist settings for this display.
    pub key: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ValueKind {
    Continuous,
    NonContinuous,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VcpValue {
    pub code: u8,
    pub name: String,
    pub current: u16,
    /// Meaningless for non-continuous codes; a UI must consult `kind` before
    /// rendering this as a range bound.
    pub max: u16,
    pub kind: ValueKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VersionInfo {
    pub protocol: u32,
    pub daemon: String,
}

// ── Method params ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetParams {
    /// Display selector: numeric id, or a case-insensitive product substring.
    pub display: String,
    /// VCP code by name ("brightness") or number ("0x10").
    pub code: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetParams {
    pub display: String,
    pub code: String,
    pub value: u16,
    /// Read the value back and report whether it actually stuck.
    ///
    /// DDC writes are fire-and-forget: the protocol has no acknowledgement, so a
    /// write "succeeding" only means the bytes went out. Monitors routinely
    /// advertise and accept codes they do not implement — the dev-bench MB169CK
    /// advertises 32 codes and honours writes to 2. Costs one extra read.
    #[serde(default)]
    pub verify: bool,
}

/// Result of `monitor.set`. `displays` is the number written, which callers
/// report to users — a selector of "all" writes more than one.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SetResult {
    pub displays: usize,
    /// Ids where `verify` was requested and the read-back did **not** match —
    /// i.e. the display ignored the write. Empty when `verify` was not asked
    /// for, so an empty list is not evidence the write landed.
    #[serde(default)]
    pub ignored: Vec<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapsParams {
    pub display: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapsResult {
    pub raw: Option<String>,
    pub vcp_codes: Vec<u8>,
    pub mccs_version: Option<String>,
    pub unknown_sections: Vec<String>,
    /// Advertised value lists for enumerated codes, as (code, values) — used to
    /// build pickers (Input Source, Color Preset, Orientation, …). Only codes
    /// with a non-empty value list appear.
    #[serde(default)]
    pub value_lists: Vec<(u8, Vec<u8>)>,
}

// ── Profiles ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileSummary {
    pub name: String,
    /// None when the profile exists but could not be parsed.
    pub displays: Option<usize>,
}

/// What happened to one setting in a profile application.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApplyStatus {
    /// Written and confirmed by read-back.
    Applied,
    /// Written, read back, and the display did not change — it does not
    /// implement this code despite accepting the write.
    Ignored,
    /// Written, but not confirmed: either verification was not requested, or the
    /// display would not report the value back. Not a claim of success.
    Unverified,
    /// The profile names a display that is not currently attached. Normal for
    /// dock/undock profiles, not a failure.
    NotConnected,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApplyOutcome {
    /// None when the display was not connected.
    pub display: Option<u32>,
    pub selector: String,
    pub code: u8,
    pub name: String,
    pub value: u16,
    pub status: ApplyStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApplyResult {
    pub profile: String,
    pub outcomes: Vec<ApplyOutcome>,
}

impl ApplyResult {
    pub fn count(&self, status: ApplyStatus) -> usize {
        self.outcomes.iter().filter(|o| o.status == status).count()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileApplyParams {
    pub name: String,
    #[serde(default)]
    pub verify: bool,
    /// Required when the profile writes destructive codes.
    #[serde(default)]
    pub yes: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileSaveParams {
    pub name: String,
    /// Codes to capture, by name or number. Empty means the default set.
    #[serde(default)]
    pub codes: Vec<String>,
    #[serde(default)]
    pub force: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileNameParams {
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuleInfo {
    pub name: String,
    pub trigger: String,
    pub action: String,
    pub enabled: bool,
    pub force: bool,
}

/// Reported when a rule fires. `ok` false means the action errored — the rule
/// still fired, so this is not the same as "did not match".
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuleFired {
    pub name: String,
    pub ok: bool,
    pub detail: String,
}

pub mod method {
    pub const VERSION: &str = "protocol.version";
    pub const LIST: &str = "monitors.list";
    pub const GET: &str = "monitor.get";
    pub const SET: &str = "monitor.set";
    pub const CAPS: &str = "monitor.caps";
    pub const PROFILE_LIST: &str = "profiles.list";
    pub const PROFILE_APPLY: &str = "profiles.apply";
    pub const PROFILE_SAVE: &str = "profiles.save";
    pub const PROFILE_DELETE: &str = "profiles.delete";
    pub const PROFILE_SHOW: &str = "profiles.show";
    pub const RULES_LIST: &str = "rules.list";
    pub const RULES_RELOAD: &str = "rules.reload";
    pub const RULES_TICK: &str = "rules.tick";
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Golden shape: the daemon and any third-party client must agree on these
    /// field names. Changing them is a protocol break and needs a version bump.
    #[test]
    fn request_serializes_to_expected_json() {
        let r = Request::new(1, "monitor.get", Some(serde_json::json!({"display":"3"})));
        let s = serde_json::to_string(&r).unwrap();
        assert_eq!(
            s,
            r#"{"jsonrpc":"2.0","id":1,"method":"monitor.get","params":{"display":"3"}}"#
        );
    }

    #[test]
    fn params_are_omitted_when_absent() {
        let r = Request::new(7, "monitors.list", None);
        assert_eq!(
            serde_json::to_string(&r).unwrap(),
            r#"{"jsonrpc":"2.0","id":7,"method":"monitors.list"}"#
        );
    }

    #[test]
    fn success_response_has_no_error_key() {
        let r = Response::ok(1, serde_json::json!({"ok":true}));
        assert_eq!(
            serde_json::to_string(&r).unwrap(),
            r#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#
        );
    }

    #[test]
    fn error_response_has_no_result_key() {
        let r = Response::err(2, INVALID_PARAMS, "bad");
        assert_eq!(
            serde_json::to_string(&r).unwrap(),
            r#"{"jsonrpc":"2.0","id":2,"error":{"code":-32602,"message":"bad"}}"#
        );
    }

    #[test]
    fn control_path_uses_lowercase_names() {
        assert_eq!(
            serde_json::to_string(&ControlPath::Ddc).unwrap(),
            r#""ddc""#
        );
        assert_eq!(
            serde_json::to_string(&ValueKind::NonContinuous).unwrap(),
            r#""noncontinuous""#
        );
    }

    /// Framing is newline-delimited, so no serialized frame may contain one.
    #[test]
    fn serialized_frames_contain_no_newlines() {
        let r = Response::err(1, INTERNAL_ERROR, "line one\nline two");
        let s = serde_json::to_string(&r).unwrap();
        assert!(!s.contains('\n'), "newline would corrupt framing: {s}");
    }
}
