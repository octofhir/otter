//! CPU profiler using sampling

use parking_lot::Mutex;
use serde::Serialize;
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// CPU profiler
pub struct CpuProfiler {
    /// Sampling interval
    #[allow(dead_code)]
    interval: Duration,
    /// Collected samples
    samples: Mutex<Vec<StackSample>>,
    /// Is profiling active
    active: Mutex<bool>,
    /// Start time
    start_time: Mutex<Option<Instant>>,
}

/// A single stack sample
#[derive(Debug, Clone, Serialize)]
pub struct StackSample {
    /// Timestamp (microseconds from start)
    pub timestamp_us: u64,
    /// Thread ID
    pub thread_id: u64,
    /// Stack frames (bottom to top)
    pub frames: Vec<StackFrame>,
}

/// A single stack frame
#[derive(Debug, Clone, Serialize)]
pub struct StackFrame {
    /// Function name
    pub function: String,
    /// Source file
    pub file: Option<String>,
    /// Line number
    pub line: Option<u32>,
    /// Column number
    pub column: Option<u32>,
}

/// CPU profile result
#[derive(Debug, Serialize)]
pub struct CpuProfile {
    /// Duration of profiling
    pub duration_us: u64,
    /// Total samples collected
    pub sample_count: usize,
    /// Stack samples
    pub samples: Vec<StackSample>,
    /// Aggregated call tree
    pub call_tree: CallTreeNode,
}

/// Call tree node for aggregated view
#[derive(Debug, Default, Serialize)]
pub struct CallTreeNode {
    /// Function name
    pub name: String,
    /// Self time (microseconds)
    pub self_time: u64,
    /// Total time (self + children)
    pub total_time: u64,
    /// Number of hits
    pub hits: u64,
    /// Children
    pub children: Vec<CallTreeNode>,
}

impl CpuProfiler {
    /// Create a new CPU profiler
    pub fn new() -> Self {
        Self {
            interval: Duration::from_micros(1000), // 1ms default
            samples: Mutex::new(Vec::new()),
            active: Mutex::new(false),
            start_time: Mutex::new(None),
        }
    }

    /// Create with custom interval
    pub fn with_interval(interval: Duration) -> Self {
        Self {
            interval,
            ..Self::new()
        }
    }

    /// Start profiling
    pub fn start(&self) {
        let mut active = self.active.lock();
        if !*active {
            *active = true;
            *self.start_time.lock() = Some(Instant::now());
            self.samples.lock().clear();
        }
    }

    /// Stop profiling and return results
    pub fn stop(&self) -> CpuProfile {
        let mut active = self.active.lock();
        *active = false;

        let start_time = self.start_time.lock().take();
        let duration = start_time.map(|t| t.elapsed()).unwrap_or_default();

        let samples = std::mem::take(&mut *self.samples.lock());
        let sample_count = samples.len();

        CpuProfile {
            duration_us: duration.as_micros() as u64,
            sample_count,
            call_tree: Self::build_call_tree(&samples),
            samples,
        }
    }

    /// Record a sample (called from VM during execution)
    pub fn record_sample(&self, frames: Vec<StackFrame>) {
        if !*self.active.lock() {
            return;
        }

        let timestamp = self
            .start_time
            .lock()
            .map(|t| t.elapsed().as_micros() as u64)
            .unwrap_or(0);

        let sample = StackSample {
            timestamp_us: timestamp,
            thread_id: thread_id(),
            frames,
        };

        self.samples.lock().push(sample);
    }

    /// Build call tree from samples
    fn build_call_tree(samples: &[StackSample]) -> CallTreeNode {
        let mut root = CallTreeNode {
            name: "(root)".to_string(),
            ..Default::default()
        };

        // Build tree by aggregating samples
        let mut frame_counts: HashMap<String, u64> = HashMap::new();

        for sample in samples {
            for frame in &sample.frames {
                *frame_counts.entry(frame.function.clone()).or_insert(0) += 1;
            }
        }

        // Convert to children (simplified - real impl would be hierarchical)
        for (name, hits) in frame_counts {
            root.children.push(CallTreeNode {
                name,
                hits,
                ..Default::default()
            });
        }

        root
    }

    /// Export to Chrome DevTools trace format
    pub fn to_chrome_trace(&self) -> serde_json::Value {
        let samples = self.samples.lock();
        let mut events = Vec::new();

        for sample in samples.iter() {
            for frame in &sample.frames {
                events.push(serde_json::json!({
                    "name": frame.function,
                    "cat": "function",
                    "ph": "X",
                    "ts": sample.timestamp_us,
                    "dur": 1000, // 1ms duration assumed
                    "pid": 1,
                    "tid": sample.thread_id,
                }));
            }
        }

        serde_json::json!({
            "traceEvents": events
        })
    }
}

impl Default for CpuProfiler {
    fn default() -> Self {
        Self::new()
    }
}

/// Get current thread ID as u64
fn thread_id() -> u64 {
    // ThreadId doesn't have as_u64() in stable Rust, use hash instead
    use std::hash::{Hash, Hasher};
    let id = std::thread::current().id();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    id.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_profiler_lifecycle() {
        let profiler = CpuProfiler::new();
        profiler.start();

        profiler.record_sample(vec![StackFrame {
            function: "test".to_string(),
            file: None,
            line: None,
            column: None,
        }]);

        let profile = profiler.stop();
        assert_eq!(profile.sample_count, 1);
    }

    #[test]
    fn test_chrome_trace_export() {
        let profiler = CpuProfiler::new();
        profiler.start();

        profiler.record_sample(vec![StackFrame {
            function: "foo".to_string(),
            file: Some("test.js".to_string()),
            line: Some(10),
            column: Some(5),
        }]);

        let trace = profiler.to_chrome_trace();
        let events = trace["traceEvents"].as_array().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["name"], "foo");
    }
}
