//! HTTP parsing for LLM API traffic
//!
//! This module handles HTTP/1.1 and HTTP/2 parsing to extract
//! LLM request/response data.

pub mod protocol;
pub mod providers;
pub mod utils;

// Re-export main types
pub use protocol::{Http2Parser, Http11Parser, ProtocolParser};
pub use providers::{ConfigurableProvider, ProviderRegistry};
