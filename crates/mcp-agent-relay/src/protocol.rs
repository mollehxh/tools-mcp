use mcp_agent_tool_contracts::{CallIdentity, ToolOutput, ToolRequest, frozen_tool_contracts};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt::Write as _;

pub const RELAY_PROTOCOL: u16 = 2;
pub const RESULT_CONTRACT_VERSION: u16 = 1;
pub const ERROR_CONTRACT_VERSION: u16 = 1;
pub const RELAY_SUBPROTOCOL: &str = "mcp-agent-relay.v2";
pub const MAX_FRAME_BYTES: usize = 1 << 20;

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct ConnectionId(String);

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct InvocationId(String);

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Platform {
    Macos,
    Windows,
    LinuxVps,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Register {
    pub min_protocol: u16,
    pub max_protocol: u16,
    pub tool_schema_digest: String,
    pub result_contract_version: u16,
    pub error_contract_version: u16,
    pub system_skill_manifest_digest: String,
    pub platform: Platform,
    pub workspace_id: String,
    pub containment_posture: String,
    pub launch_instance_id: String,
    pub connection_epoch: u64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct RelayFrame {
    pub protocol: u16,
    pub connection_id: ConnectionId,
    pub generation: u64,
    pub sequence: u64,
    #[serde(flatten)]
    pub payload: RelayPayload,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RelayPayload {
    Register {
        registration: Register,
    },
    Registered,
    Heartbeat {
        monotonic_millis: u64,
    },
    Call {
        invocation_id: InvocationId,
        deadline_unix_millis: u64,
        payload_digest: String,
        identity: CallIdentity,
        request: ToolRequest,
    },
    Cancel {
        invocation_id: InvocationId,
    },
    Result {
        invocation_id: InvocationId,
        output: ToolOutput,
    },
    Error {
        invocation_id: Option<InvocationId>,
        code: String,
        message: String,
        dispatched: Option<bool>,
    },
    Revoked {
        reason: String,
    },
    Shutdown {
        reason: String,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum RelayError {
    #[error("relay frame exceeds {MAX_FRAME_BYTES} bytes")]
    Oversized,
    #[error("relay frame is malformed")]
    Malformed(#[source] serde_json::Error),
    #[error("relay protocol {0} is unsupported")]
    UnsupportedProtocol(u16),
}

#[must_use]
pub fn new_connection_id() -> ConnectionId {
    ConnectionId(random_id())
}

#[must_use]
pub fn new_invocation_id() -> InvocationId {
    InvocationId(random_id())
}

fn random_id() -> String {
    let mut bytes = [0_u8; 16];
    rand::rng().fill_bytes(&mut bytes);
    hex_bytes(&bytes)
}

/// Encodes one frame after enforcing the fixed wire bound.
///
/// # Errors
///
/// Returns an error when serialization fails or the frame is oversized.
pub fn encode_frame(frame: &RelayFrame) -> Result<Vec<u8>, RelayError> {
    let encoded = serde_json::to_vec(frame).map_err(RelayError::Malformed)?;
    if encoded.len() > MAX_FRAME_BYTES {
        return Err(RelayError::Oversized);
    }
    Ok(encoded)
}

/// Decodes one bounded protocol-v1 frame.
///
/// # Errors
///
/// Returns an error for oversized, malformed, or incompatible frames.
pub fn decode_frame(encoded: &[u8]) -> Result<RelayFrame, RelayError> {
    if encoded.len() > MAX_FRAME_BYTES {
        return Err(RelayError::Oversized);
    }
    let frame: RelayFrame = serde_json::from_slice(encoded).map_err(RelayError::Malformed)?;
    if frame.protocol != RELAY_PROTOCOL {
        return Err(RelayError::UnsupportedProtocol(frame.protocol));
    }
    Ok(frame)
}

#[must_use]
/// Hashes a typed request for authenticated relay correlation.
///
/// # Panics
///
/// Panics only if an in-memory typed request cannot be serialized.
pub fn payload_digest(request: &ToolRequest) -> String {
    let bytes = serde_json::to_vec(request).expect("typed request serialization must succeed");
    hex_digest(&bytes)
}

#[must_use]
/// Hashes the audited five-tool schema.
///
/// # Panics
///
/// Panics only if the checked-in typed contract fixture cannot be serialized.
pub fn tool_schema_digest() -> String {
    let bytes =
        serde_json::to_vec(frozen_tool_contracts()).expect("frozen contracts must serialize");
    hex_digest(&bytes)
}

pub(crate) fn compatible_registration(registration: &Register, platform: Platform) -> bool {
    registration.min_protocol <= RELAY_PROTOCOL
        && registration.max_protocol >= RELAY_PROTOCOL
        && registration.tool_schema_digest == tool_schema_digest()
        && registration.result_contract_version == RESULT_CONTRACT_VERSION
        && registration.error_contract_version == ERROR_CONTRACT_VERSION
        && valid_sha256_digest(&registration.system_skill_manifest_digest)
        && !registration.workspace_id.is_empty()
        && registration.platform == platform
        && valid_containment(registration.platform, &registration.containment_posture)
        && !registration.launch_instance_id.is_empty()
        && registration.connection_epoch > 0
}

pub(crate) fn valid_call_identity(identity: &CallIdentity) -> bool {
    [
        identity.principal_fingerprint.as_str(),
        identity.session_fingerprint.as_str(),
    ]
    .into_iter()
    .all(|value| !value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control))
}

fn valid_sha256_digest(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn valid_containment(platform: Platform, posture: &str) -> bool {
    matches!(
        (platform, posture),
        (Platform::Macos, "macos-seatbelt-verified")
            | (Platform::Windows, "windows-restricted-token-job-verified")
            | (Platform::LinuxVps, "rootless-podman-verified")
    )
}

fn hex_digest(bytes: &[u8]) -> String {
    hex_bytes(&Sha256::digest(bytes))
}

fn hex_bytes(bytes: &[u8]) -> String {
    bytes.iter().fold(
        String::with_capacity(bytes.len() * 2),
        |mut output, byte| {
            write!(output, "{byte:02x}").expect("writing to a String cannot fail");
            output
        },
    )
}
