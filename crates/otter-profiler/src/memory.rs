//! Memory profiler and heap snapshots

use parking_lot::Mutex;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

/// Callback type for getting heap info from GC
pub type HeapInfoProvider = Arc<dyn Fn() -> HeapInfo + Send + Sync>;

/// Heap information from GC
#[derive(Debug, Clone, Default)]
pub struct HeapInfo {
    /// Total allocated bytes
    pub total_allocated: usize,
    /// Object counts by type
    pub objects_by_type: HashMap<String, TypeStats>,
    /// Total object count
    pub object_count: usize,
}

/// Memory profiler
pub struct MemoryProfiler {
    /// Heap snapshots
    snapshots: Mutex<Vec<HeapSnapshot>>,
    /// Provider for heap information
    heap_provider: Option<HeapInfoProvider>,
    /// Start time for timestamps
    start_time: Instant,
}

/// Heap snapshot
#[derive(Debug, Clone, Serialize)]
pub struct HeapSnapshot {
    /// Timestamp (microseconds from profiler start)
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
            heap_provider: None,
            start_time: Instant::now(),
        }
    }

    /// Create memory profiler with heap provider
    pub fn with_heap_provider(provider: HeapInfoProvider) -> Self {
        Self {
            snapshots: Mutex::new(Vec::new()),
            heap_provider: Some(provider),
            start_time: Instant::now(),
        }
    }

    /// Set heap info provider
    pub fn set_heap_provider(&mut self, provider: HeapInfoProvider) {
        self.heap_provider = Some(provider);
    }

    /// Take a heap snapshot
    pub fn take_snapshot(&self) -> HeapSnapshot {
        let timestamp = self.start_time.elapsed().as_micros() as u64;

        let snapshot = if let Some(provider) = &self.heap_provider {
            let info = provider();
            HeapSnapshot {
                timestamp_us: timestamp,
                total_size: info.total_allocated,
                objects_by_type: info.objects_by_type,
                object_count: info.object_count,
            }
        } else {
            // Fallback when no provider is set
            HeapSnapshot {
                timestamp_us: timestamp,
                total_size: 0,
                objects_by_type: HashMap::new(),
                object_count: 0,
            }
        };

        self.snapshots.lock().push(snapshot.clone());
        snapshot
    }

    /// Get all snapshots
    pub fn snapshots(&self) -> Vec<HeapSnapshot> {
        self.snapshots.lock().clone()
    }

    /// Clear all snapshots
    pub fn clear(&self) {
        self.snapshots.lock().clear();
    }

    /// Compare two snapshots (for leak detection)
    pub fn diff(&self, before: &HeapSnapshot, after: &HeapSnapshot) -> HeapSnapshotDiff {
        let size_delta = after.total_size as i64 - before.total_size as i64;
        let count_delta = after.object_count as i64 - before.object_count as i64;

        let mut type_changes = HashMap::new();
        for (type_name, after_stats) in &after.objects_by_type {
            let before_stats = before.objects_by_type.get(type_name);
            let count_before = before_stats.map(|s| s.count).unwrap_or(0);
            let size_before = before_stats.map(|s| s.size).unwrap_or(0);

            type_changes.insert(
                type_name.clone(),
                TypeStatsDiff {
                    count_delta: after_stats.count as i64 - count_before as i64,
                    size_delta: after_stats.size as i64 - size_before as i64,
                },
            );
        }

        HeapSnapshotDiff {
            time_delta_us: after.timestamp_us - before.timestamp_us,
            size_delta,
            count_delta,
            type_changes,
        }
    }

    /// Export to Chrome DevTools heap snapshot format
    pub fn to_heapsnapshot(&self, snapshot: &HeapSnapshot) -> serde_json::Value {
        // Build nodes array - each node has 5 fields
        let mut nodes = Vec::new();
        let mut strings = vec!["(root)".to_string()];
        let mut node_id = 1u64;

        // Add root node
        nodes.extend([0, 0, 0, 0, snapshot.object_count as i64]); // type=hidden, name=0, id=0, size=0, edge_count

        // Add type nodes
        for (type_name, stats) in &snapshot.objects_by_type {
            let name_idx = strings.len();
            strings.push(type_name.clone());
            nodes.extend([
                1,                 // type = object
                name_idx as i64,   // name index
                node_id as i64,    // id
                stats.size as i64, // self_size
                0,                 // edge_count
            ]);
            node_id += 1;
        }

        serde_json::json!({
            "snapshot": {
                "meta": {
                    "node_fields": ["type", "name", "id", "self_size", "edge_count"],
                    "node_types": [["hidden", "object", "string", "number"], "string", "number", "number", "number"],
                    "edge_fields": ["type", "name_or_index", "to_node"],
                    "edge_types": [["context", "element", "property"], "string_or_number", "node"],
                },
                "node_count": 1 + snapshot.objects_by_type.len(),
                "edge_count": 0,
            },
            "nodes": nodes,
            "edges": [],
            "strings": strings,
        })
    }
}

/// Difference between two heap snapshots
#[derive(Debug, Clone, Serialize)]
pub struct HeapSnapshotDiff {
    /// Time difference in microseconds
    pub time_delta_us: u64,
    /// Total size change in bytes (can be negative)
    pub size_delta: i64,
    /// Object count change (can be negative)
    pub count_delta: i64,
    /// Changes per type
    pub type_changes: HashMap<String, TypeStatsDiff>,
}

/// Statistics diff for a type
#[derive(Debug, Clone, Serialize)]
pub struct TypeStatsDiff {
    /// Count change
    pub count_delta: i64,
    /// Size change
    pub size_delta: i64,
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
    fn test_take_snapshot_without_provider() {
        let profiler = MemoryProfiler::new();
        let snapshot = profiler.take_snapshot();
        assert_eq!(snapshot.object_count, 0);
        assert_eq!(snapshot.total_size, 0);
    }

    #[test]
    fn test_take_snapshot_with_provider() {
        let profiler = MemoryProfiler::with_heap_provider(Arc::new(|| HeapInfo {
            total_allocated: 1024,
            object_count: 10,
            objects_by_type: {
                let mut map = HashMap::new();
                map.insert(
                    "Object".to_string(),
                    TypeStats {
                        count: 5,
                        size: 512,
                    },
                );
                map.insert(
                    "String".to_string(),
                    TypeStats {
                        count: 5,
                        size: 512,
                    },
                );
                map
            },
        }));

        let snapshot = profiler.take_snapshot();
        assert_eq!(snapshot.total_size, 1024);
        assert_eq!(snapshot.object_count, 10);
        assert_eq!(snapshot.objects_by_type.len(), 2);
    }

    #[test]
    fn test_snapshot_diff() {
        let profiler = MemoryProfiler::new();

        let before = HeapSnapshot {
            timestamp_us: 0,
            total_size: 1000,
            object_count: 10,
            objects_by_type: HashMap::new(),
        };

        let after = HeapSnapshot {
            timestamp_us: 1000,
            total_size: 1500,
            object_count: 15,
            objects_by_type: HashMap::new(),
        };

        let diff = profiler.diff(&before, &after);
        assert_eq!(diff.size_delta, 500);
        assert_eq!(diff.count_delta, 5);
    }

    #[test]
    fn test_heapsnapshot_export() {
        let profiler = MemoryProfiler::new();
        let snapshot = profiler.take_snapshot();
        let export = profiler.to_heapsnapshot(&snapshot);
        assert!(export["snapshot"]["meta"]["node_fields"].is_array());
        assert!(export["nodes"].is_array());
        assert!(export["strings"].is_array());
    }
}
