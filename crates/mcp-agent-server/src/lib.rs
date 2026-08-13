//! Streamable HTTP MCP adapter for the transport-neutral application crates.

pub mod context;
pub mod handler;
pub mod http;
pub mod result;
pub mod stub;

pub use context::ApplicationContext;
pub use handler::AgentHandler;
pub use stub::StubServer;
