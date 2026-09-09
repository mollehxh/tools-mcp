//! Bounded, versioned worker relay protocol. MCP itself never crosses this boundary.

mod gateway_socket;
mod outbound;
mod protocol;
mod socket;
mod tls;
mod worker;

pub use gateway_socket::{
    GatewaySocketConfig, GatewaySocketConfigError, GatewaySocketError, RemoteBackend,
    accept_gateway_authenticated,
};
pub use outbound::{
    OutboundWorkerError, ReconnectConfig, run_outbound_worker, run_reconnecting_outbound_worker,
};
pub use protocol::{
    ConnectionId, ERROR_CONTRACT_VERSION, InvocationId, MAX_FRAME_BYTES, Platform, RELAY_PROTOCOL,
    RELAY_SUBPROTOCOL, RESULT_CONTRACT_VERSION, Register, RelayError, RelayFrame, RelayPayload,
    decode_frame, encode_frame, new_connection_id, new_invocation_id, payload_digest,
    tool_schema_digest,
};
pub use socket::{SocketError, serve_authenticated};
pub use tls::{MtlsAcceptor, PeerResolver, TlsError, certificate_fingerprint};
pub use worker::{AuthenticatedPeer, RelayWorker, WorkerConfig};
