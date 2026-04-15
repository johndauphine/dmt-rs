//! AI-powered type mapping and database assistance.
//!
//! This module provides LLM-backed type mapping for cross-database migrations.
//! When the static type mapper returns an unknown/fallback type, the AI mapper
//! consults a configured LLM provider to determine the correct target type.
//!
//! Results are cached persistently to avoid repeated API calls.

mod cache;
mod config;
mod mapper;
mod prompt;
mod provider;

pub use cache::TypeCache;
pub use config::{AiConfig, AiProvider, GlobalConfig};
pub use mapper::AiTypeMapper;
pub use prompt::PromptContext;
pub use provider::{AiProviderClient, create_provider};
