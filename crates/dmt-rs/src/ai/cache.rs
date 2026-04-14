use crate::error::{MigrateError, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use tracing::{debug, info, warn};

/// Cache key uniquely identifying a type mapping request.
#[derive(Debug, Clone, Hash, Eq, PartialEq, Serialize, Deserialize)]
pub struct CacheKey {
    pub source_db: String,
    pub target_db: String,
    pub source_type: String,
    pub max_length: i32,
    pub precision: i32,
    pub scale: i32,
}

impl CacheKey {
    pub fn new(
        source_db: &str,
        target_db: &str,
        source_type: &str,
        max_length: i32,
        precision: i32,
        scale: i32,
    ) -> Self {
        Self {
            source_db: source_db.to_lowercase(),
            target_db: target_db.to_lowercase(),
            source_type: source_type.to_lowercase(),
            max_length,
            precision,
            scale,
        }
    }

    /// String key for JSON serialization.
    fn to_string_key(&self) -> String {
        format!(
            "{}|{}|{}|{}|{}|{}",
            self.source_db, self.target_db, self.source_type,
            self.max_length, self.precision, self.scale
        )
    }

    /// Parse from string key.
    fn from_string_key(s: &str) -> Option<Self> {
        let parts: Vec<&str> = s.split('|').collect();
        if parts.len() != 6 {
            return None;
        }
        Some(Self {
            source_db: parts[0].to_string(),
            target_db: parts[1].to_string(),
            source_type: parts[2].to_string(),
            max_length: parts[3].parse().ok()?,
            precision: parts[4].parse().ok()?,
            scale: parts[5].parse().ok()?,
        })
    }
}

/// Cached type mapping result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheEntry {
    pub target_type: String,
    pub created_at: String,
}

/// In-memory + persistent disk cache for AI type mapping results.
pub struct TypeCache {
    memory: RwLock<HashMap<CacheKey, CacheEntry>>,
    file_path: PathBuf,
}

impl TypeCache {
    /// Load cache from disk, or create empty if file doesn't exist or is corrupt.
    pub fn load(path: &Path) -> Self {
        let memory = if path.exists() {
            match std::fs::read_to_string(path) {
                Ok(content) => {
                    match serde_json::from_str::<HashMap<String, CacheEntry>>(&content) {
                        Ok(disk_map) => {
                            let mut mem = HashMap::new();
                            for (key_str, entry) in disk_map {
                                if let Some(key) = CacheKey::from_string_key(&key_str) {
                                    mem.insert(key, entry);
                                }
                            }
                            info!("Loaded {} entries from AI type cache {:?}", mem.len(), path);
                            mem
                        }
                        Err(e) => {
                            warn!("Failed to parse AI type cache {:?}: {}. Starting fresh.", path, e);
                            HashMap::new()
                        }
                    }
                }
                Err(e) => {
                    warn!("Failed to read AI type cache {:?}: {}. Starting fresh.", path, e);
                    HashMap::new()
                }
            }
        } else {
            HashMap::new()
        };

        Self {
            memory: RwLock::new(memory),
            file_path: path.to_path_buf(),
        }
    }

    /// Look up a cached type mapping (sync, non-blocking read).
    pub fn get(&self, key: &CacheKey) -> Option<CacheEntry> {
        self.memory.read().unwrap().get(key).cloned()
    }

    /// Insert a single entry and flush to disk.
    pub fn insert(&self, key: CacheKey, entry: CacheEntry) -> Result<()> {
        {
            let mut mem = self.memory.write().unwrap();
            mem.insert(key, entry);
        }
        self.flush()
    }

    /// Insert a batch of entries and flush once.
    pub fn insert_batch(&self, entries: Vec<(CacheKey, CacheEntry)>) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        {
            let mut mem = self.memory.write().unwrap();
            for (key, entry) in entries {
                mem.insert(key, entry);
            }
        }
        self.flush()
    }

    /// Check if a key exists in the cache.
    pub fn contains(&self, key: &CacheKey) -> bool {
        self.memory.read().unwrap().contains_key(key)
    }

    /// Number of entries in the cache.
    pub fn len(&self) -> usize {
        self.memory.read().unwrap().len()
    }

    /// Flush the in-memory cache to disk.
    fn flush(&self) -> Result<()> {
        let mem = self.memory.read().unwrap();
        let disk_map: HashMap<String, &CacheEntry> = mem
            .iter()
            .map(|(k, v)| (k.to_string_key(), v))
            .collect();

        // Ensure parent directory exists with secure permissions
        if let Some(parent) = self.file_path.parent() {
            if !parent.exists() {
                std::fs::create_dir_all(parent)?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
                }
            }
        }

        let json = serde_json::to_string_pretty(&disk_map)
            .map_err(|e| MigrateError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("Failed to serialize type cache: {}", e),
            )))?;

        // Atomic write: write to temp file, then rename
        let tmp_path = self.file_path.with_extension("json.tmp");
        std::fs::write(&tmp_path, &json)?;
        std::fs::rename(&tmp_path, &self.file_path)?;

        // Set cache file to 600 (owner read/write only)
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&self.file_path, std::fs::Permissions::from_mode(0o600))?;
        }

        debug!("Flushed {} entries to AI type cache {:?}", mem.len(), self.file_path);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_cache_key_roundtrip() {
        let key = CacheKey::new("mssql", "postgres", "hierarchyid", 0, 0, 0);
        let s = key.to_string_key();
        let parsed = CacheKey::from_string_key(&s).unwrap();
        assert_eq!(key, parsed);
    }

    #[test]
    fn test_cache_insert_and_get() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("cache.json");
        let cache = TypeCache::load(&path);

        let key = CacheKey::new("mssql", "postgres", "hierarchyid", 0, 0, 0);
        let entry = CacheEntry {
            target_type: "text".to_string(),
            created_at: "2026-04-14T00:00:00Z".to_string(),
        };

        assert!(cache.get(&key).is_none());
        cache.insert(key.clone(), entry.clone()).unwrap();
        assert_eq!(cache.get(&key).unwrap().target_type, "text");

        // Verify persisted to disk
        let cache2 = TypeCache::load(&path);
        assert_eq!(cache2.get(&key).unwrap().target_type, "text");
    }

    #[test]
    fn test_cache_batch_insert() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("cache.json");
        let cache = TypeCache::load(&path);

        let entries = vec![
            (
                CacheKey::new("mssql", "postgres", "hierarchyid", 0, 0, 0),
                CacheEntry { target_type: "text".to_string(), created_at: "2026-04-14".to_string() },
            ),
            (
                CacheKey::new("mssql", "postgres", "geography", 0, 0, 0),
                CacheEntry { target_type: "text".to_string(), created_at: "2026-04-14".to_string() },
            ),
        ];

        cache.insert_batch(entries).unwrap();
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn test_cache_corrupt_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("cache.json");
        std::fs::write(&path, "not json").unwrap();

        let cache = TypeCache::load(&path);
        assert_eq!(cache.len(), 0); // Should start fresh
    }
}
