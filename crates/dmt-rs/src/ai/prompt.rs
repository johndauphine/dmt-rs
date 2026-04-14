/// Build a type mapping prompt for the AI provider.
pub fn build_type_mapping_prompt(
    source_db: &str,
    target_db: &str,
    source_type: &str,
    max_length: i32,
    precision: i32,
    scale: i32,
) -> (String, String) {
    let system = "You are a database type mapping expert. Your job is to map database column types \
        from one database engine to another. Return ONLY the target type name with appropriate \
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

    user.push_str("\n\nTarget type:");

    (system, user)
}

/// Validate and clean an AI response to extract just the type name.
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

    Some(cleaned.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_prompt() {
        let (system, user) = build_type_mapping_prompt(
            "mssql", "postgres", "hierarchyid", 0, 0, 0,
        );
        assert!(system.contains("database type mapping expert"));
        assert!(user.contains("hierarchyid"));
        assert!(user.contains("mssql"));
        assert!(user.contains("postgres"));
        // No length/precision/scale lines when 0
        assert!(!user.contains("Max length"));
    }

    #[test]
    fn test_build_prompt_with_params() {
        let (_, user) = build_type_mapping_prompt(
            "mssql", "postgres", "decimal", 0, 18, 2,
        );
        assert!(user.contains("Precision: 18"));
        assert!(user.contains("Scale: 2"));
    }

    #[test]
    fn test_clean_response() {
        assert_eq!(clean_type_response("text"), Some("text".to_string()));
        assert_eq!(clean_type_response("  text  "), Some("text".to_string()));
        assert_eq!(clean_type_response("`text`"), Some("text".to_string()));
        assert_eq!(clean_type_response("\"varchar(255)\""), Some("varchar(255)".to_string()));
        assert_eq!(clean_type_response(""), None);
        assert_eq!(clean_type_response("The best type is text"), None);
        assert_eq!(clean_type_response("line1\nline2"), None);
    }
}
