//! Memory profiler and heap snapshots

use parking_lot::Mutex;
use serde::Serialize;
use std::collections::HashMap;

/// Memory profiler
pub struct MemoryProfiler {
    /// Heap snapshots
    snapshots: Mutex<Vec<HeapSnapshot>>,
}

/// Heap snapshot
#[derive(Debug, Clone, Serialize)]
pub struct HeapSnapshot {
    /// Timestamp
    pub timestamp_us: u64,
    /// Total heap size
    pub total_size: usize,
    /// Objects by type
    pub objects_by_type: HashMap<String, TypeStats>,
    /// Object count
    pub object_count: usize,
}

/// Statistics for a type
#[derive(Debug, Clone, Default, Serialize)]
pub struct TypeStats {
    /// Number of instances
    pub count: usize,
    /// Total size in bytes
    pub size: usize,
}

impl MemoryProfiler {
    /// Create new memory profiler
    pub fn new() -> Self {
        Self {
            snapshots: Mutex::new(Vec::new()),
        }
    }

    /// Take a heap snapshot
    pub fn take_snapshot(&self) -> HeapSnapshot {
        // This will be connected to GC heap later
        let snapshot = HeapSnapshot {
            timestamp_us: 0,
            total_size: 0,
            objects_by_type: HashMap::new(),
            object_count: 0,
        };

        self.snapshots.lock().push(snapshot.clone());
        snapshot
    }

    /// Get all snapshots
    pub fn snapshots(&self) -> Vec<HeapSnapshot> {
        self.snapshots.lock().clone()
    }

    /// Export to Chrome DevTools heap snapshot format
    pub fn to_heapsnapshot(&self, snapshot: &HeapSnapshot) -> serde_json::Value {
        serde_json::json!({
            "snapshot": {
                "meta": {
                    "node_fields": ["type", "name", "id", "self_size", "edge_count"],
                    "node_types": [["hidden", "object", "string", "number"], "string", "number", "number", "number"],
                },
                "node_count": snapshot.object_count,
                "edge_count": 0,
            },
            "nodes": [],
            "edges": [],
            "strings": []
        })
    }
}

impl Default for MemoryProfiler {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_take_snapshot() {
        let profiler = MemoryProfiler::new();
        let snapshot = profiler.take_snapshot();
        assert_eq!(snapshot.object_count, 0);
    }

    #[test]
    fn test_heapsnapshot_export() {
        let profiler = MemoryProfiler::new();
        let snapshot = profiler.take_snapshot();
        let export = profiler.to_heapsnapshot(&snapshot);
        assert!(export["snapshot"]["meta"]["node_fields"].is_array());
    }
}
