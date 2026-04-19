//! AI-powered error diagnosis.
//!
//! Mirrors the Go `dmt` error-diagnosis feature
//! (`internal/driver/ai_errordiag.go`). When a migration fails during DDL
//! creation or data transfer, the caught error is sent to the configured
//! LLM with schema context; the structured response (root cause +
//! suggestions + confidence + category) is cached by error-message hash
//! and emitted through a pluggable handler.
//!
//! Activation: whenever AI is configured. No separate CLI flag — parity
//! with Go dmt, which gates on `aiMapper != nil`.

use crate::ai::provider::AiProviderClient;
use crate::core::schema::Column;
use crate::error::{MigrateError, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::{Arc, OnceLock, RwLock};
use tracing::{debug, warn};

/// AI-generated analysis of a migration error.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorDiagnosis {
    /// Root cause (1–2 sentences).
    pub cause: String,
    /// Actionable fixes, ordered by likely effectiveness.
    pub suggestions: Vec<String>,
    /// `high` | `medium` | `low`.
    pub confidence: String,
    /// `type_mismatch` | `constraint` | `permission` | `connection` | `data_quality` | `other`.
    pub category: String,
}

/// Context passed to the diagnoser for a single error.
#[derive(Debug, Clone, Default)]
pub struct ErrorContext {
    pub error_message: String,
    pub table_name: String,
    pub table_schema: String,
    pub columns: Vec<Column>,
    pub source_db_type: String,
    pub target_db_type: String,
    pub target_mode: String,
}

/// AI error diagnoser. Caches results by a 128-bit truncated SHA-256 of
/// the error message (parity with Go dmt's `digest[:16]`). The truncation
/// is intentional: 128 bits of collision resistance is ample for an
/// in-process dedup cache where keys are short error strings.
pub struct ErrorDiagnoser {
    provider: Arc<dyn AiProviderClient>,
    cache: RwLock<HashMap<String, ErrorDiagnosis>>,
}

impl ErrorDiagnoser {
    pub fn new(provider: Arc<dyn AiProviderClient>) -> Self {
        Self {
            provider,
            cache: RwLock::new(HashMap::new()),
        }
    }

    /// Analyze an error and return a structured diagnosis.
    pub async fn diagnose(&self, ctx: &ErrorContext) -> Result<ErrorDiagnosis> {
        let key = hash_error(&ctx.error_message);

        if let Some(cached) = self.cache.read().unwrap().get(&key).cloned() {
            debug!("AI error diagnosis: cache hit for error hash {}", &key[..8]);
            return Ok(cached);
        }

        let (system, user) = build_prompt(ctx);

        debug!(
            "AI error diagnosis: analyzing error for table {}.{}",
            ctx.table_schema, ctx.table_name
        );

        let raw = self
            .provider
            .complete_text(&system, &user, 1024)
            .await
            .map_err(|e| MigrateError::Config(format!("AI diagnosis failed: {}", e)))?;

        let diagnosis = parse_response(&raw)?;

        debug!(
            "AI error diagnosis: category={}, confidence={}",
            diagnosis.category, diagnosis.confidence
        );

        self.cache.write().unwrap().insert(key, diagnosis.clone());
        Ok(diagnosis)
    }

    pub fn cache_size(&self) -> usize {
        self.cache.read().unwrap().len()
    }

    pub fn clear_cache(&self) {
        self.cache.write().unwrap().clear();
    }
}

/// Handler callback for diagnosis output. The TUI registers one to render
/// diagnoses as boxed messages; the CLI falls back to logging.
pub type DiagnosisHandler = Arc<dyn Fn(&ErrorDiagnosis) + Send + Sync>;

fn handler_slot() -> &'static RwLock<Option<DiagnosisHandler>> {
    static SLOT: OnceLock<RwLock<Option<DiagnosisHandler>>> = OnceLock::new();
    SLOT.get_or_init(|| RwLock::new(None))
}

/// Register a callback to receive diagnosis events. Pass `None` to
/// unregister and fall back to logging.
pub fn set_diagnosis_handler(handler: Option<DiagnosisHandler>) {
    *handler_slot().write().unwrap() = handler;
}

/// Dispatch a diagnosis to the registered handler, or log it as fallback.
///
/// The handler is cloned out of the lock before invocation so that a
/// handler which calls [`set_diagnosis_handler`] (or otherwise needs the
/// write lock) cannot deadlock, and so the read lock isn't held for the
/// duration of handler execution.
pub fn emit_diagnosis(diag: &ErrorDiagnosis) {
    let handler = handler_slot().read().unwrap().clone();
    if let Some(h) = handler {
        h(diag);
    } else {
        warn!("\n{}", diag.format_boxed());
    }
}

/// Convenience wrapper for diagnose-then-emit from a `tokio::spawn`'d
/// task. The caller is responsible for pre-fetching the diagnoser and
/// constructing the context before the spawn (so the method doesn't need
/// `&Orchestrator` access inside the task).
pub async fn diagnose_and_emit(diagnoser: Arc<ErrorDiagnoser>, ctx: ErrorContext) {
    match diagnoser.diagnose(&ctx).await {
        Ok(d) => emit_diagnosis(&d),
        Err(e) => debug!("AI error diagnosis unavailable: {}", e),
    }
}

/// Format an error and its full `source()` chain into a single string.
/// The top-level `Display` impl on `MigrateError` often hides the real
/// detail (e.g., `"db error"` vs the underlying `"permission denied for
/// schema public"`), so we walk the chain to give the LLM everything.
pub fn format_error_chain(err: &(dyn std::error::Error + 'static)) -> String {
    let mut parts: Vec<String> = vec![err.to_string()];
    let mut cursor: Option<&(dyn std::error::Error + 'static)> = err.source();
    while let Some(e) = cursor {
        let s = e.to_string();
        // Skip duplicates — some error types repeat the outer message.
        if parts.last().map(|p| p != &s).unwrap_or(true) {
            parts.push(s);
        }
        cursor = e.source();
    }
    parts.join(": ")
}

fn hash_error(msg: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(msg.as_bytes());
    let digest = hasher.finalize();
    hex::encode(&digest[..16])
}

fn build_prompt(ctx: &ErrorContext) -> (String, String) {
    let system = "You are a database migration error analyst. Analyze the given \
                  error and return ONLY a JSON object with fields: cause (string, \
                  1-2 sentences), suggestions (array of 2-3 actionable strings), \
                  confidence (\"high\"|\"medium\"|\"low\"), category \
                  (\"type_mismatch\"|\"constraint\"|\"permission\"|\"connection\"|\"data_quality\"|\"other\"). \
                  No markdown, no prose outside the JSON."
        .to_string();

    let mut user = String::new();
    let _ = writeln!(user, "=== ERROR ===");
    let _ = writeln!(user, "{}", ctx.error_message);
    let _ = writeln!(user);
    let _ = writeln!(user, "=== CONTEXT ===");
    let _ = writeln!(user, "Source DB: {}", ctx.source_db_type);
    let _ = writeln!(user, "Target DB: {}", ctx.target_db_type);
    let _ = writeln!(
        user,
        "Table: {}.{}",
        ctx.table_schema, ctx.table_name
    );
    if !ctx.target_mode.is_empty() {
        let _ = writeln!(user, "Mode: {}", ctx.target_mode);
    }

    if !ctx.columns.is_empty() {
        let _ = writeln!(user);
        let _ = writeln!(user, "Columns (name: source_type):");
        let max_cols = 20;
        for (i, col) in ctx.columns.iter().enumerate() {
            if i >= max_cols {
                let _ = writeln!(
                    user,
                    "  ... and {} more columns",
                    ctx.columns.len() - max_cols
                );
                break;
            }
            let type_str = if col.max_length > 0 {
                format!("{}({})", col.data_type, col.max_length)
            } else if col.precision > 0 && col.scale > 0 {
                format!("{}({},{})", col.data_type, col.precision, col.scale)
            } else if col.precision > 0 {
                format!("{}({})", col.data_type, col.precision)
            } else {
                col.data_type.clone()
            };
            let nullable = if col.is_nullable { "" } else { " NOT NULL" };
            let _ = writeln!(user, "  {}: {}{}", col.name, type_str, nullable);
        }
    }

    let _ = writeln!(user);
    let _ = writeln!(user, "=== OUTPUT ===");
    user.push_str(
        r#"Respond with ONLY a JSON object (no markdown, no explanation):
{
  "cause": "brief root cause explanation (1-2 sentences)",
  "suggestions": ["actionable fix 1", "actionable fix 2", "actionable fix 3"],
  "confidence": "high|medium|low",
  "category": "type_mismatch|constraint|permission|connection|data_quality|other"
}"#,
    );

    (system, user)
}

fn parse_response(raw: &str) -> Result<ErrorDiagnosis> {
    let cleaned = raw
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();

    let mut diag: ErrorDiagnosis = serde_json::from_str(cleaned).map_err(|e| {
        MigrateError::Config(format!(
            "invalid AI diagnosis JSON: {} (response: {})",
            e,
            truncate(cleaned, 100)
        ))
    })?;

    if diag.cause.trim().is_empty() {
        return Err(MigrateError::Config(
            "AI diagnosis missing 'cause' field".into(),
        ));
    }
    if diag.suggestions.is_empty() {
        return Err(MigrateError::Config(
            "AI diagnosis missing 'suggestions' field".into(),
        ));
    }

    diag.confidence = match diag.confidence.to_ascii_lowercase().as_str() {
        "high" | "medium" | "low" => diag.confidence.to_ascii_lowercase(),
        _ => "medium".to_string(),
    };
    diag.category = match diag.category.to_ascii_lowercase().as_str() {
        "type_mismatch" | "constraint" | "permission" | "connection" | "data_quality" | "other" => {
            diag.category.to_ascii_lowercase()
        }
        _ => "other".to_string(),
    };

    Ok(diag)
}

/// Char-boundary-safe truncation. Byte-slicing with `&s[..max]` can panic
/// on non-ASCII input if `max` falls inside a multi-byte UTF-8 sequence,
/// so we walk `char_indices` and stop at the last char that fits.
fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let end = s
        .char_indices()
        .map(|(i, ch)| i + ch.len_utf8())
        .take_while(|&end| end <= max)
        .last()
        .unwrap_or(0);
    format!("{}...", &s[..end])
}

impl ErrorDiagnosis {
    /// Plain text rendering.
    pub fn format(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(out, "AI Error Diagnosis");
        let _ = writeln!(out);
        let _ = writeln!(out, "Cause: {}", self.cause);
        let _ = writeln!(out);
        let _ = writeln!(out, "Suggestions:");
        for (i, s) in self.suggestions.iter().enumerate() {
            let _ = writeln!(out, "  {}. {}", i + 1, s);
        }
        let _ = writeln!(out);
        let _ = writeln!(
            out,
            "Confidence: {}  |  Category: {}",
            self.confidence, self.category
        );
        out
    }

    /// Unicode-boxed rendering, 72 chars wide. Matches Go dmt FormatBox().
    pub fn format_boxed(&self) -> String {
        let width = 72usize;
        let mut out = String::new();

        let write_padded = |out: &mut String, content: &str| {
            let truncated = if content.len() > width - 4 {
                truncate(content, width - 7)
            } else {
                content.to_string()
            };
            let padding = width.saturating_sub(4 + truncated.chars().count());
            let _ = writeln!(out, "│ {}{} │", truncated, " ".repeat(padding));
        };

        let title = " AI Error Diagnosis ";
        let left_pad = (width - 2 - title.len()) / 2;
        let right_pad = width - 2 - title.len() - left_pad;
        let _ = writeln!(
            out,
            "┌{}{}{}┐",
            "─".repeat(left_pad),
            title,
            "─".repeat(right_pad)
        );

        write_padded(&mut out, "");
        write_padded(&mut out, &format!("Cause: {}", self.cause));
        write_padded(&mut out, "");
        write_padded(&mut out, "Suggestions:");
        for (i, s) in self.suggestions.iter().enumerate() {
            write_padded(&mut out, &format!("  {}. {}", i + 1, s));
        }
        write_padded(&mut out, "");
        write_padded(
            &mut out,
            &format!("Confidence: {}  |  Category: {}", self.confidence, self.category),
        );

        let _ = write!(out, "└{}┘", "─".repeat(width - 2));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Serializes tests that mutate the global diagnosis handler so they
    /// don't trip over each other under `cargo test`'s default parallelism.
    static HANDLER_TEST_LOCK: Mutex<()> = Mutex::new(());

    /// RAII guard that captures the current global handler on construction
    /// and restores it on drop, so a test can install a handler without
    /// leaking state into sibling tests.
    struct HandlerGuard {
        prev: Option<DiagnosisHandler>,
    }

    impl HandlerGuard {
        fn new() -> Self {
            Self {
                prev: handler_slot().read().unwrap().clone(),
            }
        }
    }

    impl Drop for HandlerGuard {
        fn drop(&mut self) {
            *handler_slot().write().unwrap() = self.prev.take();
        }
    }

    #[test]
    fn parses_clean_json() {
        let raw = r#"{"cause":"x","suggestions":["a","b"],"confidence":"HIGH","category":"TYPE_MISMATCH"}"#;
        let d = parse_response(raw).unwrap();
        assert_eq!(d.cause, "x");
        assert_eq!(d.suggestions, vec!["a", "b"]);
        assert_eq!(d.confidence, "high");
        assert_eq!(d.category, "type_mismatch");
    }

    #[test]
    fn parses_markdown_fenced_json() {
        let raw = "```json\n{\"cause\":\"c\",\"suggestions\":[\"s1\"],\"confidence\":\"low\",\"category\":\"other\"}\n```";
        let d = parse_response(raw).unwrap();
        assert_eq!(d.cause, "c");
        assert_eq!(d.confidence, "low");
    }

    #[test]
    fn normalizes_unknown_enum_values() {
        let raw = r#"{"cause":"x","suggestions":["a"],"confidence":"wibble","category":"nonsense"}"#;
        let d = parse_response(raw).unwrap();
        assert_eq!(d.confidence, "medium");
        assert_eq!(d.category, "other");
    }

    #[test]
    fn rejects_missing_fields() {
        let raw = r#"{"cause":"","suggestions":["a"],"confidence":"high","category":"other"}"#;
        assert!(parse_response(raw).is_err());
        let raw = r#"{"cause":"x","suggestions":[],"confidence":"high","category":"other"}"#;
        assert!(parse_response(raw).is_err());
    }

    #[test]
    fn format_plain_contains_all_parts() {
        let d = ErrorDiagnosis {
            cause: "missing column".into(),
            suggestions: vec!["add col".into(), "skip table".into()],
            confidence: "high".into(),
            category: "constraint".into(),
        };
        let s = d.format();
        assert!(s.contains("AI Error Diagnosis"));
        assert!(s.contains("missing column"));
        assert!(s.contains("1. add col"));
        assert!(s.contains("2. skip table"));
        assert!(s.contains("Confidence: high"));
        assert!(s.contains("Category: constraint"));
    }

    #[test]
    fn format_boxed_has_consistent_width() {
        let d = ErrorDiagnosis {
            cause: "x".into(),
            suggestions: vec!["a".into()],
            confidence: "high".into(),
            category: "other".into(),
        };
        let s = d.format_boxed();
        // First and last lines must be top/bottom borders of the fixed width.
        let first = s.lines().next().unwrap();
        let last = s.lines().last().unwrap();
        assert!(first.starts_with('┌'));
        assert!(first.ends_with('┐'));
        assert!(last.starts_with('└'));
        assert!(last.ends_with('┘'));
    }

    #[test]
    fn hash_is_stable_and_16_bytes_hex() {
        let h1 = hash_error("some error");
        let h2 = hash_error("some error");
        let h3 = hash_error("different error");
        assert_eq!(h1, h2);
        assert_ne!(h1, h3);
        assert_eq!(h1.len(), 32); // 16 bytes hex = 32 chars
    }

    #[test]
    fn handler_callback_fires_when_registered() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let _serial = HANDLER_TEST_LOCK.lock().unwrap();
        let _guard = HandlerGuard::new();

        let counter = Arc::new(AtomicUsize::new(0));
        let c = counter.clone();
        set_diagnosis_handler(Some(Arc::new(move |_: &ErrorDiagnosis| {
            c.fetch_add(1, Ordering::SeqCst);
        })));

        let d = ErrorDiagnosis {
            cause: "x".into(),
            suggestions: vec!["a".into()],
            confidence: "high".into(),
            category: "other".into(),
        };
        emit_diagnosis(&d);
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn format_error_chain_walks_sources() {
        use std::error::Error;
        use std::fmt;

        #[derive(Debug)]
        struct Outer(Box<dyn Error + Send + Sync>);
        #[derive(Debug)]
        struct Middle(Box<dyn Error + Send + Sync>);
        #[derive(Debug)]
        struct Inner(&'static str);
        impl fmt::Display for Outer {
            fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
                write!(f, "outer")
            }
        }
        impl fmt::Display for Middle {
            fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
                write!(f, "middle")
            }
        }
        impl fmt::Display for Inner {
            fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
                write!(f, "{}", self.0)
            }
        }
        impl Error for Outer {
            fn source(&self) -> Option<&(dyn Error + 'static)> {
                Some(self.0.as_ref())
            }
        }
        impl Error for Middle {
            fn source(&self) -> Option<&(dyn Error + 'static)> {
                Some(self.0.as_ref())
            }
        }
        impl Error for Inner {}

        let err = Outer(Box::new(Middle(Box::new(Inner("permission denied")))));
        let s = format_error_chain(&err);
        assert_eq!(s, "outer: middle: permission denied");
    }

    #[test]
    fn format_error_chain_skips_duplicate_messages() {
        use std::error::Error;
        use std::fmt;

        #[derive(Debug)]
        struct Dup(&'static str, Option<Box<dyn Error + Send + Sync>>);
        impl fmt::Display for Dup {
            fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
                write!(f, "{}", self.0)
            }
        }
        impl Error for Dup {
            fn source(&self) -> Option<&(dyn Error + 'static)> {
                self.1.as_deref().map(|e| e as &(dyn Error + 'static))
            }
        }

        let err = Dup("db error", Some(Box::new(Dup("db error", None))));
        let s = format_error_chain(&err);
        assert_eq!(s, "db error");
    }

    #[test]
    fn truncate_handles_multibyte_chars() {
        // "caf\u{00e9}" — e-acute is 2 bytes UTF-8. Truncating at max=4 would
        // panic on byte-slicing mid-sequence; char-safe version must stop at 3.
        let s = "caf\u{00e9}";
        assert_eq!(s.len(), 5);
        let t = truncate(s, 4);
        // Result is "caf..." — the e-acute doesn't fit in 4 bytes after "caf".
        assert!(t.starts_with("caf"));
        assert!(t.ends_with("..."));
    }
}
