//! Configuration-driven LLM Provider System
//!
//! This module provides a flexible, config-driven approach to LLM provider detection
//! and usage parsing. Instead of hardcoding each provider, users can define providers
//! via configuration.
//!
//! # Example Configuration (YAML)
//! ```yaml
//! providers:
//!   - name: openai
//!     hosts: ["api.openai.com"]
//!     paths: ["/chat/completions", "/v1/completions"]
//!     response:
//!       usage_path: "usage"
//!       prompt_tokens: "prompt_tokens"
//!       completion_tokens: "completion_tokens"
//!       model_path: "model"
//!     request_extractor: "messages"
//! ```

mod config;
mod request;
mod usage;

pub use config::{ProviderConfig, ProviderRegistry, RequestExtractorType, ResponseConfig};
pub use request::RequestExtractor;
pub use usage::ConfigurableProvider;
