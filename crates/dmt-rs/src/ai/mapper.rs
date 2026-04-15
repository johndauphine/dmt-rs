use crate::ai::cache::{CacheEntry, CacheKey, TypeCache};
use crate::ai::prompt::PromptContext;
use crate::ai::provider::AiProviderClient;
use crate::core::schema::{Column, Table};
use crate::core::traits::{ColumnMapping, TypeMapper, TypeMapping};
use crate::error::Result;
use std::collections::HashSet;
use std::sync::Arc;
use tracing::{debug, info, warn};

/// AI-powered type mapper that wraps a static mapper with LLM fallback.
///
/// The static mapper is tried first. If it returns a fallback/unknown type,
/// the AI cache is consulted. The cache is populated during a warm-up phase
/// before DDL generation.
pub struct AiTypeMapper {
    inner: Arc<dyn TypeMapper>,
    cache: Arc<TypeCache>,
}

impl AiTypeMapper {
    /// Create a new AI type mapper wrapping an existing static mapper.
    pub fn new(inner: Arc<dyn TypeMapper>, cache: Arc<TypeCache>) -> Self {
        Self { inner, cache }
    }

    /// Pre-populate cache for all unknown types found in the schema.
    /// Called after schema extraction, before DDL generation.
    ///
    /// `context` provides database-specific prompt guidance from the source
    /// and target dialects (e.g., "prefer nvarchar for MSSQL text").
    pub async fn warm_up(
        &self,
        tables: &[Table],
        provider: &dyn AiProviderClient,
        context: &PromptContext,
    ) -> Result<()> {
        // Collect all unique type signatures across all tables
        let mut unknown_keys: HashSet<CacheKey> = HashSet::new();

        for table in tables {
            for col in &table.columns {
                let mapping = self.inner.map_type(
                    &col.data_type,
                    col.max_length,
                    col.precision,
                    col.scale,
                );

                if mapping.is_fallback {
                    let key = CacheKey::new(
                        self.inner.source_dialect(),
                        self.inner.target_dialect(),
                        &col.data_type,
                        col.max_length,
                        col.precision,
                        col.scale,
                    );

                    // Skip if already cached
                    if !self.cache.contains(&key) {
                        unknown_keys.insert(key);
                    }
                }
            }
        }

        if unknown_keys.is_empty() {
            debug!("AI warm-up: no unknown types found, skipping");
            return Ok(());
        }

        info!(
            "AI warm-up: resolving {} unknown type(s) via {}→{}",
            unknown_keys.len(),
            self.inner.source_dialect(),
            self.inner.target_dialect()
        );

        // Call AI for each unknown type
        let mut new_entries = Vec::new();
        for key in &unknown_keys {
            match provider
                .map_type(
                    &key.source_db,
                    &key.target_db,
                    &key.source_type,
                    key.max_length,
                    key.precision,
                    key.scale,
                    context,
                )
                .await
            {
                Ok(target_type) => {
                    info!(
                        "AI mapped: {} ({}) -> {} ({})",
                        key.source_type, key.source_db, target_type, key.target_db
                    );
                    new_entries.push((
                        key.clone(),
                        CacheEntry {
                            target_type,
                            created_at: chrono::Utc::now().to_rfc3339(),
                        },
                    ));
                }
                Err(e) => {
                    warn!(
                        "AI failed to map type '{}' ({}→{}): {}. Using static fallback.",
                        key.source_type, key.source_db, key.target_db, e
                    );
                }
            }
        }

        if !new_entries.is_empty() {
            info!("AI warm-up: caching {} new type mapping(s)", new_entries.len());
            self.cache.insert_batch(new_entries)?;
        }

        Ok(())
    }
}

impl TypeMapper for AiTypeMapper {
    fn source_dialect(&self) -> &str {
        self.inner.source_dialect()
    }

    fn target_dialect(&self) -> &str {
        self.inner.target_dialect()
    }

    fn map_type(
        &self,
        data_type: &str,
        max_length: i32,
        precision: i32,
        scale: i32,
    ) -> TypeMapping {
        // Step 1: Try static mapper
        let result = self.inner.map_type(data_type, max_length, precision, scale);

        // Step 2: If fallback, check AI cache
        if result.is_fallback {
            let key = CacheKey::new(
                self.inner.source_dialect(),
                self.inner.target_dialect(),
                data_type,
                max_length,
                precision,
                scale,
            );

            if let Some(entry) = self.cache.get(&key) {
                debug!(
                    "AI cache hit: {} -> {} ({}→{})",
                    data_type, entry.target_type,
                    self.inner.source_dialect(),
                    self.inner.target_dialect()
                );
                return TypeMapping {
                    target_type: entry.target_type,
                    is_lossy: true,
                    warning: Some(format!("AI-mapped from '{}' (static mapper had no mapping)", data_type)),
                    is_fallback: false, // AI resolved it
                };
            }

            // Cache miss: warn but return the static fallback
            debug!(
                "AI cache miss for '{}' ({}→{}), using static fallback: {}",
                data_type,
                self.inner.source_dialect(),
                self.inner.target_dialect(),
                result.target_type
            );
        }

        result
    }

    fn map_column(&self, col: &Column) -> ColumnMapping {
        let type_mapping = self.map_type(&col.data_type, col.max_length, col.precision, col.scale);
        ColumnMapping {
            name: col.name.clone(),
            target_type: type_mapping.target_type,
            is_nullable: col.is_nullable,
            warning: type_mapping.warning,
        }
    }
}
