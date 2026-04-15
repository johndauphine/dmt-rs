use crate::ai::config::{AiConfig, AiProvider};
use crate::ai::prompt::{build_type_mapping_prompt, clean_type_response, PromptContext};
use crate::error::{MigrateError, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tracing::debug;

/// Trait for AI provider clients.
#[async_trait]
pub trait AiProviderClient: Send + Sync {
    /// Map a source database type to a target type using AI.
    async fn map_type(
        &self,
        source_db: &str,
        target_db: &str,
        source_type: &str,
        max_length: i32,
        precision: i32,
        scale: i32,
        context: &PromptContext,
    ) -> Result<String>;
}

/// Create an AI provider client from config.
pub fn create_provider(config: &AiConfig) -> Result<Box<dyn AiProviderClient>> {
    let api_key = config.resolve_api_key();
    let model = config.model_or_default();
    let base_url = config.base_url_or_default();

    match config.provider {
        AiProvider::Anthropic => {
            if api_key.is_empty() {
                return Err(MigrateError::Config(
                    "AI provider 'anthropic' requires an api_key (set ai.api_key or ANTHROPIC_API_KEY env var)".into()
                ));
            }
            Ok(Box::new(AnthropicClient { api_key, model, base_url, http: reqwest::Client::new() }))
        }
        AiProvider::OpenAI => {
            if api_key.is_empty() {
                return Err(MigrateError::Config(
                    "AI provider 'openai' requires an api_key (set ai.api_key or OPENAI_API_KEY env var)".into()
                ));
            }
            Ok(Box::new(OpenAiCompatibleClient { api_key, model, base_url, http: reqwest::Client::new() }))
        }
        AiProvider::Ollama => {
            Ok(Box::new(OpenAiCompatibleClient {
                api_key: String::new(),
                model,
                base_url,
                http: reqwest::Client::new(),
            }))
        }
        AiProvider::LMStudio => {
            Ok(Box::new(OpenAiCompatibleClient {
                api_key: String::new(),
                model,
                base_url,
                http: reqwest::Client::new(),
            }))
        }
    }
}

// --- Anthropic Client ---

struct AnthropicClient {
    api_key: String,
    model: String,
    base_url: String,
    http: reqwest::Client,
}

#[derive(Serialize)]
struct AnthropicRequest {
    model: String,
    max_tokens: u32,
    system: String,
    messages: Vec<AnthropicMessage>,
}

#[derive(Serialize)]
struct AnthropicMessage {
    role: String,
    content: String,
}

#[derive(Deserialize)]
struct AnthropicResponse {
    content: Vec<AnthropicContent>,
}

#[derive(Deserialize)]
struct AnthropicContent {
    text: String,
}

#[async_trait]
impl AiProviderClient for AnthropicClient {
    async fn map_type(
        &self,
        source_db: &str,
        target_db: &str,
        source_type: &str,
        max_length: i32,
        precision: i32,
        scale: i32,
        context: &PromptContext,
    ) -> Result<String> {
        let (system, user) = build_type_mapping_prompt(
            source_db, target_db, source_type, max_length, precision, scale, context,
        );

        let request = AnthropicRequest {
            model: self.model.clone(),
            max_tokens: 100,
            system,
            messages: vec![AnthropicMessage {
                role: "user".to_string(),
                content: user,
            }],
        };

        let url = format!("{}/v1/messages", self.base_url);

        debug!("AI type mapping request: {} {} -> {}", source_type, source_db, target_db);

        let response = self.http
            .post(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&request)
            .timeout(std::time::Duration::from_secs(30))
            .send()
            .await
            .map_err(|e| MigrateError::Config(format!("AI API request failed: {}", e)))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(MigrateError::Config(format!(
                "AI API returned {}: {}", status, body
            )));
        }

        let body: AnthropicResponse = response
            .json()
            .await
            .map_err(|e| MigrateError::Config(format!("AI API response parse error: {}", e)))?;

        let raw = body.content.first()
            .map(|c| c.text.clone())
            .unwrap_or_default();

        clean_type_response(&raw).ok_or_else(|| {
            MigrateError::Config(format!(
                "AI returned invalid type mapping for '{}': '{}'", source_type, raw
            ))
        })
    }
}

// --- OpenAI-Compatible Client (OpenAI, Ollama, LM Studio) ---

struct OpenAiCompatibleClient {
    api_key: String,
    model: String,
    base_url: String,
    http: reqwest::Client,
}

#[derive(Serialize)]
struct OpenAiRequest {
    model: String,
    max_tokens: u32,
    messages: Vec<OpenAiMessage>,
}

#[derive(Serialize)]
struct OpenAiMessage {
    role: String,
    content: String,
}

#[derive(Deserialize)]
struct OpenAiResponse {
    choices: Vec<OpenAiChoice>,
}

#[derive(Deserialize)]
struct OpenAiChoice {
    message: OpenAiResponseMessage,
}

#[derive(Deserialize)]
struct OpenAiResponseMessage {
    content: String,
}

#[async_trait]
impl AiProviderClient for OpenAiCompatibleClient {
    async fn map_type(
        &self,
        source_db: &str,
        target_db: &str,
        source_type: &str,
        max_length: i32,
        precision: i32,
        scale: i32,
        context: &PromptContext,
    ) -> Result<String> {
        let (system, user) = build_type_mapping_prompt(
            source_db, target_db, source_type, max_length, precision, scale, context,
        );

        let request = OpenAiRequest {
            model: self.model.clone(),
            max_tokens: 100,
            messages: vec![
                OpenAiMessage { role: "system".to_string(), content: system },
                OpenAiMessage { role: "user".to_string(), content: user },
            ],
        };

        let url = format!("{}/v1/chat/completions", self.base_url);

        debug!("AI type mapping request: {} {} -> {}", source_type, source_db, target_db);

        let mut req = self.http
            .post(&url)
            .header("content-type", "application/json")
            .json(&request)
            .timeout(std::time::Duration::from_secs(30));

        if !self.api_key.is_empty() {
            req = req.header("authorization", format!("Bearer {}", self.api_key));
        }

        let response = req
            .send()
            .await
            .map_err(|e| MigrateError::Config(format!("AI API request failed: {}", e)))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(MigrateError::Config(format!(
                "AI API returned {}: {}", status, body
            )));
        }

        let body: OpenAiResponse = response
            .json()
            .await
            .map_err(|e| MigrateError::Config(format!("AI API response parse error: {}", e)))?;

        let raw = body.choices.first()
            .map(|c| c.message.content.clone())
            .unwrap_or_default();

        clean_type_response(&raw).ok_or_else(|| {
            MigrateError::Config(format!(
                "AI returned invalid type mapping for '{}': '{}'", source_type, raw
            ))
        })
    }
}
