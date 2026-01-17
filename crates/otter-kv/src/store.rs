//! KV store implementation using redb

use redb::{Database, ReadableTable, TableDefinition};
use std::path::Path;
use std::sync::Arc;

/// Error type for KV operations
#[derive(Debug)]
pub enum KvError {
    /// Database error
    Database(String),
    /// Serialization error
    Serialization(String),
    /// Key not found
    NotFound(String),
    /// Invalid path
    InvalidPath(String),
}

impl std::fmt::Display for KvError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KvError::Database(msg) => write!(f, "Database error: {}", msg),
            KvError::Serialization(msg) => write!(f, "Serialization error: {}", msg),
            KvError::NotFound(key) => write!(f, "Key not found: {}", key),
            KvError::InvalidPath(msg) => write!(f, "Invalid path: {}", msg),
        }
    }
}

impl std::error::Error for KvError {}

/// Result type for KV operations
pub type KvResult<T> = Result<T, KvError>;

// Table definition for the KV store
const TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("kv");

/// KV store backed by redb
pub struct KvStore {
    db: Arc<Database>,
    is_memory: bool,
}

impl KvStore {
    /// Open or create a KV store
    ///
    /// # Arguments
    /// * `path` - Database path. Use `:memory:` for in-memory database,
    ///           or a file path for persistent storage
    pub fn open(path: &str) -> KvResult<Self> {
        let is_memory = path == ":memory:";

        let db = if is_memory {
            // Create a temporary file for in-memory simulation
            // redb doesn't support true in-memory mode, so we use a temp file
            let temp_path =
                std::env::temp_dir().join(format!("otter-kv-{}.redb", std::process::id()));
            Database::create(&temp_path).map_err(|e| KvError::Database(e.to_string()))?
        } else {
            let path = Path::new(path);
            if let Some(parent) = path.parent() {
                if !parent.exists() {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| KvError::InvalidPath(e.to_string()))?;
                }
            }
            Database::create(path).map_err(|e| KvError::Database(e.to_string()))?
        };

        // Initialize the table
        {
            let write_txn = db
                .begin_write()
                .map_err(|e| KvError::Database(e.to_string()))?;
            {
                let _ = write_txn.open_table(TABLE);
            }
            write_txn
                .commit()
                .map_err(|e| KvError::Database(e.to_string()))?;
        }

        Ok(Self {
            db: Arc::new(db),
            is_memory,
        })
    }

    /// Set a value for a key
    pub fn set(&self, key: &str, value: &serde_json::Value) -> KvResult<()> {
        let serialized =
            serde_json::to_vec(value).map_err(|e| KvError::Serialization(e.to_string()))?;

        let write_txn = self
            .db
            .begin_write()
            .map_err(|e| KvError::Database(e.to_string()))?;
        {
            let mut table = write_txn
                .open_table(TABLE)
                .map_err(|e| KvError::Database(e.to_string()))?;
            table
                .insert(key, serialized.as_slice())
                .map_err(|e| KvError::Database(e.to_string()))?;
        }
        write_txn
            .commit()
            .map_err(|e| KvError::Database(e.to_string()))?;

        Ok(())
    }

    /// Get a value by key
    pub fn get(&self, key: &str) -> KvResult<Option<serde_json::Value>> {
        let read_txn = self
            .db
            .begin_read()
            .map_err(|e| KvError::Database(e.to_string()))?;
        let table = read_txn
            .open_table(TABLE)
            .map_err(|e| KvError::Database(e.to_string()))?;

        match table.get(key) {
            Ok(Some(guard)) => {
                let bytes = guard.value();
                let value: serde_json::Value = serde_json::from_slice(bytes)
                    .map_err(|e| KvError::Serialization(e.to_string()))?;
                Ok(Some(value))
            }
            Ok(None) => Ok(None),
            Err(e) => Err(KvError::Database(e.to_string())),
        }
    }

    /// Delete a key
    pub fn delete(&self, key: &str) -> KvResult<bool> {
        // First check if key exists
        let existed = self.has(key)?;

        if existed {
            let write_txn = self
                .db
                .begin_write()
                .map_err(|e| KvError::Database(e.to_string()))?;
            {
                let mut table = write_txn
                    .open_table(TABLE)
                    .map_err(|e| KvError::Database(e.to_string()))?;
                let _ = table
                    .remove(key)
                    .map_err(|e| KvError::Database(e.to_string()))?;
            }
            write_txn
                .commit()
                .map_err(|e| KvError::Database(e.to_string()))?;
        }

        Ok(existed)
    }

    /// Check if a key exists
    pub fn has(&self, key: &str) -> KvResult<bool> {
        let read_txn = self
            .db
            .begin_read()
            .map_err(|e| KvError::Database(e.to_string()))?;
        let table = read_txn
            .open_table(TABLE)
            .map_err(|e| KvError::Database(e.to_string()))?;

        match table.get(key) {
            Ok(Some(_)) => Ok(true),
            Ok(None) => Ok(false),
            Err(e) => Err(KvError::Database(e.to_string())),
        }
    }

    /// Get all keys
    pub fn keys(&self) -> KvResult<Vec<String>> {
        let read_txn = self
            .db
            .begin_read()
            .map_err(|e| KvError::Database(e.to_string()))?;
        let table = read_txn
            .open_table(TABLE)
            .map_err(|e| KvError::Database(e.to_string()))?;

        let mut keys = Vec::new();
        let iter = table.iter().map_err(|e| KvError::Database(e.to_string()))?;
        for item in iter {
            let (key, _) = item.map_err(|e| KvError::Database(e.to_string()))?;
            keys.push(key.value().to_string());
        }

        Ok(keys)
    }

    /// Clear all keys
    pub fn clear(&self) -> KvResult<()> {
        let keys = self.keys()?;
        let write_txn = self
            .db
            .begin_write()
            .map_err(|e| KvError::Database(e.to_string()))?;
        {
            let mut table = write_txn
                .open_table(TABLE)
                .map_err(|e| KvError::Database(e.to_string()))?;
            for key in keys {
                table
                    .remove(key.as_str())
                    .map_err(|e| KvError::Database(e.to_string()))?;
            }
        }
        write_txn
            .commit()
            .map_err(|e| KvError::Database(e.to_string()))?;

        Ok(())
    }

    /// Get the number of keys
    pub fn len(&self) -> KvResult<usize> {
        let read_txn = self
            .db
            .begin_read()
            .map_err(|e| KvError::Database(e.to_string()))?;
        let table = read_txn
            .open_table(TABLE)
            .map_err(|e| KvError::Database(e.to_string()))?;

        // Count items by iterating (redb doesn't have a direct len() method)
        let iter = table.iter().map_err(|e| KvError::Database(e.to_string()))?;
        let count = iter.count();
        Ok(count)
    }

    /// Check if the store is empty
    pub fn is_empty(&self) -> KvResult<bool> {
        Ok(self.len()? == 0)
    }

    /// Check if this is an in-memory store
    pub fn is_memory(&self) -> bool {
        self.is_memory
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_kv_basic() {
        let store = KvStore::open(":memory:").unwrap();

        // Set and get
        store.set("key1", &json!("value1")).unwrap();
        assert_eq!(store.get("key1").unwrap(), Some(json!("value1")));

        // Has
        assert!(store.has("key1").unwrap());
        assert!(!store.has("nonexistent").unwrap());

        // Delete
        assert!(store.delete("key1").unwrap());
        assert!(!store.has("key1").unwrap());
        assert!(!store.delete("key1").unwrap());
    }

    #[test]
    fn test_kv_complex_values() {
        let store = KvStore::open(":memory:").unwrap();

        let obj = json!({
            "name": "Alice",
            "age": 30,
            "tags": ["rust", "js"],
            "nested": { "key": "value" }
        });

        store.set("user:1", &obj).unwrap();
        assert_eq!(store.get("user:1").unwrap(), Some(obj));
    }

    #[test]
    fn test_kv_keys() {
        let store = KvStore::open(":memory:").unwrap();

        store.set("a", &json!(1)).unwrap();
        store.set("b", &json!(2)).unwrap();
        store.set("c", &json!(3)).unwrap();

        let mut keys = store.keys().unwrap();
        keys.sort();
        assert_eq!(keys, vec!["a", "b", "c"]);
    }

    #[test]
    fn test_kv_clear() {
        let store = KvStore::open(":memory:").unwrap();

        store.set("a", &json!(1)).unwrap();
        store.set("b", &json!(2)).unwrap();

        assert_eq!(store.len().unwrap(), 2);

        store.clear().unwrap();

        assert_eq!(store.len().unwrap(), 0);
        assert!(store.is_empty().unwrap());
    }
}
