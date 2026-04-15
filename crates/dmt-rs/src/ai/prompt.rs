/// Context for augmenting the AI type mapping prompt with
/// database-specific guidance from source and target dialects.
#[derive(Debug, Clone, Default)]
pub struct PromptContext {
    /// Source dialect guidance (e.g., "user-defined types may be enums").
    pub source_guidance: Option<String>,
    /// Target dialect guidance (e.g., "prefer nvarchar for Unicode text").
    pub target_guidance: Option<String>,
}

/// Build a type mapping prompt for the AI provider.
///
/// The prompt is composed from:
/// 1. A generic system prompt (engine-agnostic)
/// 2. Source/target database names and type metadata
/// 3. Source dialect guidance (if provided)
/// 4. Target dialect guidance (if provided)
///
/// Dialect-specific guidance is supplied by each database's `Dialect::ai_type_guidance()`
/// implementation, keeping engine-specific rules with the engine code rather than
/// hard-coding them in the prompt.
pub fn build_type_mapping_prompt(
    source_db: &str,
    target_db: &str,
    source_type: &str,
    max_length: i32,
    precision: i32,
    scale: i32,
    context: &PromptContext,
) -> (String, String) {
    let system = "You are a database type mapping expert. Your job is to map database column types \
        from one database engine to another. \
        Preserve semantics over name similarity. Prefer lossless mappings. \
        Preserve Unicode and character semantics where possible. \
        Return ONLY the target type name with appropriate \
        parameters (e.g., 'varchar(255)', 'numeric(18,2)', 'text'). No explanation, no markdown, \
        no quotes — just the type name."
        .to_string();

    let mut user = format!(
        "Map this source type to the best equivalent in the target database.\n\n\
         Source database: {}\n\
         Target database: {}\n\
         Source type: {}",
        source_db, target_db, source_type
    );

    if max_length > 0 {
        user.push_str(&format!("\nMax length: {}", max_length));
    }
    if precision > 0 {
        user.push_str(&format!("\nPrecision: {}", precision));
    }
    if scale > 0 {
        user.push_str(&format!("\nScale: {}", scale));
    }

    // Append dialect-specific guidance
    if let Some(ref guidance) = context.source_guidance {
        user.push_str(&format!("\n\nSource guidance: {}", guidance));
    }
    if let Some(ref guidance) = context.target_guidance {
        user.push_str(&format!("\n\nTarget guidance: {}", guidance));
    }

    user.push_str("\n\nTarget type:");

    (system, user)
}

/// Validate and clean an AI response to extract just the type name.
///
/// # Security
///
/// The returned string is used in DDL generation (CREATE TABLE statements).
/// This function rejects responses containing SQL metacharacters to prevent
/// injection attacks from malicious or confused AI responses.
pub fn clean_type_response(response: &str) -> Option<String> {
    let cleaned = response
        .trim()
        .trim_matches('`')
        .trim_matches('"')
        .trim_matches('\'')
        .trim();

    // Reject empty or multi-line responses
    if cleaned.is_empty() || cleaned.contains('\n') {
        return None;
    }

    // Reject responses that are too long (not a type name)
    if cleaned.len() > 100 {
        return None;
    }

    // Reject responses that look like explanations
    if cleaned.contains("The ") || cleaned.contains("This ") || cleaned.contains("You ") {
        return None;
    }

    // Reject SQL metacharacters to prevent injection in DDL generation.
    // Valid type names contain only: alphanumeric, spaces, parentheses, commas,
    // brackets (for arrays like int[]), and periods (for schema-qualified types).
    if cleaned.contains(';') || cleaned.contains("--") || cleaned.contains("/*") {
        return None;
    }

    // Only allow characters that appear in valid type names
    let valid = cleaned.chars().all(|c| {
        c.is_alphanumeric()
            || c == ' '
            || c == '('
            || c == ')'
            || c == ','
            || c == '['
            || c == ']'
            || c == '.'
            || c == '_'
    });
    if !valid {
        return None;
    }

    Some(cleaned.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_prompt_no_guidance() {
        let ctx = PromptContext::default();
        let (system, user) = build_type_mapping_prompt(
            "mssql", "postgres", "hierarchyid", 0, 0, 0, &ctx,
        );
        assert!(system.contains("database type mapping expert"));
        assert!(system.contains("Preserve semantics"));
        assert!(user.contains("hierarchyid"));
        assert!(user.contains("mssql"));
        assert!(user.contains("postgres"));
        assert!(!user.contains("Max length"));
        assert!(!user.contains("Source guidance"));
        assert!(!user.contains("Target guidance"));
    }

    #[test]
    fn test_build_prompt_with_params() {
        let ctx = PromptContext::default();
        let (_, user) = build_type_mapping_prompt(
            "mssql", "postgres", "decimal", 0, 18, 2, &ctx,
        );
        assert!(user.contains("Precision: 18"));
        assert!(user.contains("Scale: 2"));
    }

    #[test]
    fn test_build_prompt_with_source_guidance() {
        let ctx = PromptContext {
            source_guidance: Some("User-defined types may be enums".to_string()),
            target_guidance: None,
        };
        let (_, user) = build_type_mapping_prompt(
            "postgres", "mssql", "contact_choice", 0, 0, 0, &ctx,
        );
        assert!(user.contains("Source guidance: User-defined types may be enums"));
        assert!(!user.contains("Target guidance"));
    }

    #[test]
    fn test_build_prompt_with_target_guidance() {
        let ctx = PromptContext {
            source_guidance: None,
            target_guidance: Some("Prefer nvarchar over varchar for text".to_string()),
        };
        let (_, user) = build_type_mapping_prompt(
            "postgres", "mssql", "text", 0, 0, 0, &ctx,
        );
        assert!(user.contains("Target guidance: Prefer nvarchar over varchar for text"));
        assert!(!user.contains("Source guidance"));
    }

    #[test]
    fn test_build_prompt_with_both_guidance() {
        let ctx = PromptContext {
            source_guidance: Some("Enums are text-like".to_string()),
            target_guidance: Some("Use nvarchar for Unicode".to_string()),
        };
        let (_, user) = build_type_mapping_prompt(
            "postgres", "mssql", "my_enum", 0, 0, 0, &ctx,
        );
        assert!(user.contains("Source guidance: Enums are text-like"));
        assert!(user.contains("Target guidance: Use nvarchar for Unicode"));
        // Source appears before target
        let src_pos = user.find("Source guidance").unwrap();
        let tgt_pos = user.find("Target guidance").unwrap();
        assert!(src_pos < tgt_pos);
    }

    #[test]
    fn test_clean_response_valid() {
        assert_eq!(clean_type_response("text"), Some("text".to_string()));
        assert_eq!(clean_type_response("  text  "), Some("text".to_string()));
        assert_eq!(clean_type_response("`text`"), Some("text".to_string()));
        assert_eq!(clean_type_response("\"varchar(255)\""), Some("varchar(255)".to_string()));
        assert_eq!(clean_type_response("numeric(18,2)"), Some("numeric(18,2)".to_string()));
        assert_eq!(clean_type_response("integer[]"), Some("integer[]".to_string()));
        assert_eq!(clean_type_response("double precision"), Some("double precision".to_string()));
        assert_eq!(clean_type_response("character varying(100)"), Some("character varying(100)".to_string()));
        assert_eq!(clean_type_response("bytea"), Some("bytea".to_string()));
    }

    #[test]
    fn test_clean_response_rejects_invalid() {
        assert_eq!(clean_type_response(""), None);
        assert_eq!(clean_type_response("The best type is text"), None);
        assert_eq!(clean_type_response("line1\nline2"), None);
    }

    #[test]
    fn test_clean_response_rejects_sql_injection() {
        // Semicolons (statement termination)
        assert_eq!(clean_type_response("TEXT); DROP TABLE users; --"), None);
        // SQL comments
        assert_eq!(clean_type_response("TEXT -- comment"), None);
        assert_eq!(clean_type_response("TEXT /* block */"), None);
        // Other dangerous characters
        assert_eq!(clean_type_response("TEXT' OR '1'='1"), None);
        assert_eq!(clean_type_response("TEXT; SELECT *"), None);
    }
}
