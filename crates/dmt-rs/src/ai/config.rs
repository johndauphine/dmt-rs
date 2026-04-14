use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Global application configuration (loaded from ~/.dmt-rs/dmt-rs-config.yaml).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GlobalConfig {
    /// AI configuration for type mapping and error diagnosis.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ai: Option<AiConfig>,
}

/// AI provider and model configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiConfig {
    /// API key. Supports ${env:VAR_NAME} syntax for environment variable expansion.
    #[serde(default)]
    pub api_key: String,

    /// Provider: anthropic (default), openai, ollama, lmstudio
    #[serde(default)]
    pub provider: AiProvider,

    /// Model name (optional, sensible defaults per provider)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,

    /// Base URL override (for ollama, lmstudio, or custom endpoints)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,

    /// Cache file path (default: ~/.dmt-rs/type-cache.json)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum AiProvider {
    #[default]
    Anthropic,
    #[serde(alias = "openai")]
    OpenAI,
    Ollama,
    #[serde(alias = "lmstudio", alias = "lm_studio")]
    LMStudio,
}

impl AiConfig {
    /// Resolve the API key, expanding ${env:VAR_NAME} references.
    pub fn resolve_api_key(&self) -> String {
        expand_env_vars(&self.api_key)
    }

    /// Get the cache file path, defaulting to ~/.dmt-rs/type-cache.json.
    pub fn cache_path(&self) -> PathBuf {
        if let Some(ref path) = self.cache_path {
            PathBuf::from(expand_env_vars(path))
        } else {
            default_cache_path()
        }
    }

    /// Get the default model for the configured provider.
    pub fn model_or_default(&self) -> String {
        if let Some(ref model) = self.model {
            return model.clone();
        }
        match self.provider {
            AiProvider::Anthropic => "claude-haiku-4-5-20251001".to_string(),
            AiProvider::OpenAI => "gpt-4o".to_string(),
            AiProvider::Ollama => "llama3.1".to_string(),
            AiProvider::LMStudio => "default".to_string(),
        }
    }

    /// Get the base URL for the configured provider.
    pub fn base_url_or_default(&self) -> String {
        if let Some(ref url) = self.base_url {
            return url.clone();
        }
        match self.provider {
            AiProvider::Anthropic => "https://api.anthropic.com".to_string(),
            AiProvider::OpenAI => "https://api.openai.com".to_string(),
            AiProvider::Ollama => "http://localhost:11434".to_string(),
            AiProvider::LMStudio => "http://localhost:1234".to_string(),
        }
    }
}

impl GlobalConfig {
    /// Load global config from a file path. Returns default if file doesn't exist.
    ///
    /// Warns if the file has overly permissive permissions (readable by group/others),
    /// since it may contain API keys.
    pub fn load(path: &Path) -> crate::error::Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }

        // Check file permissions on Unix — warn if group/world readable
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(metadata) = std::fs::metadata(path) {
                let mode = metadata.permissions().mode();
                if mode & 0o077 != 0 {
                    tracing::warn!(
                        "Global config {:?} has overly permissive permissions ({:o}). \
                         This file may contain API keys. Run: chmod 600 {:?}",
                        path, mode & 0o777, path
                    );
                }
            }
        }

        let content = std::fs::read_to_string(path)?;
        let config: GlobalConfig = serde_yaml::from_str(&content)
            .map_err(|e| crate::error::MigrateError::Config(
                format!("Failed to parse global config {:?}: {}", path, e)
            ))?;
        Ok(config)
    }

    /// Default global config file path: ~/.dmt-rs/dmt-rs-config.yaml
    pub fn default_path() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".dmt-rs")
            .join("dmt-rs-config.yaml")
    }

    /// Ensure the config directory exists with secure permissions (700).
    pub fn ensure_config_dir() -> crate::error::Result<PathBuf> {
        let dir = dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".dmt-rs");

        if !dir.exists() {
            std::fs::create_dir_all(&dir)?;

            // Set directory to 700 (owner only) on Unix
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
            }

            tracing::info!("Created config directory {:?} with restricted permissions", dir);
        }

        Ok(dir)
    }

    /// Write a global config file with secure permissions (600).
    pub fn save(&self, path: &Path) -> crate::error::Result<()> {
        // Ensure parent directory exists with secure permissions
        if let Some(parent) = path.parent() {
            if !parent.exists() {
                Self::ensure_config_dir()?;
            }
        }

        let content = serde_yaml::to_string(self)
            .map_err(|e| crate::error::MigrateError::Config(
                format!("Failed to serialize global config: {}", e)
            ))?;

        std::fs::write(path, &content)?;

        // Set file to 600 (owner read/write only) on Unix
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }

        Ok(())
    }
}

/// Expand ${env:VAR_NAME} references in a string.
fn expand_env_vars(s: &str) -> String {
    let mut result = s.to_string();
    // Match ${env:VAR_NAME} pattern
    while let Some(start) = result.find("${env:") {
        if let Some(end) = result[start..].find('}') {
            let var_name = &result[start + 6..start + end];
            let value = std::env::var(var_name).unwrap_or_default();
            result = format!("{}{}{}", &result[..start], value, &result[start + end + 1..]);
        } else {
            break;
        }
    }
    result
}

/// Default cache path: ~/.dmt-rs/type-cache.json
fn default_cache_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".dmt-rs")
        .join("type-cache.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_expand_env_vars() {
        std::env::set_var("DMT_TEST_KEY", "test_value");
        assert_eq!(expand_env_vars("${env:DMT_TEST_KEY}"), "test_value");
        assert_eq!(expand_env_vars("prefix_${env:DMT_TEST_KEY}_suffix"), "prefix_test_value_suffix");
        assert_eq!(expand_env_vars("no_expansion"), "no_expansion");
        assert_eq!(expand_env_vars("${env:DMT_NONEXISTENT}"), "");
        std::env::remove_var("DMT_TEST_KEY");
    }

    #[test]
    fn test_default_models() {
        let config = AiConfig {
            api_key: String::new(),
            provider: AiProvider::Anthropic,
            model: None,
            base_url: None,
            cache_path: None,
        };
        assert_eq!(config.model_or_default(), "claude-haiku-4-5-20251001");

        let config = AiConfig { provider: AiProvider::OpenAI, ..config.clone() };
        assert_eq!(config.model_or_default(), "gpt-4o");
    }

    #[test]
    fn test_deserialize_providers() {
        let yaml = "provider: anthropic";
        let config: AiConfig = serde_yaml::from_str(&format!("api_key: test\n{}", yaml)).unwrap();
        assert_eq!(config.provider, AiProvider::Anthropic);

        let yaml = "provider: openai";
        let config: AiConfig = serde_yaml::from_str(&format!("api_key: test\n{}", yaml)).unwrap();
        assert_eq!(config.provider, AiProvider::OpenAI);

        let yaml = "provider: lmstudio";
        let config: AiConfig = serde_yaml::from_str(&format!("api_key: test\n{}", yaml)).unwrap();
        assert_eq!(config.provider, AiProvider::LMStudio);
    }

    #[test]
    fn test_global_config_default_on_missing_file() {
        let config = GlobalConfig::load(Path::new("/nonexistent/path.yaml")).unwrap();
        assert!(config.ai.is_none());
    }
}
