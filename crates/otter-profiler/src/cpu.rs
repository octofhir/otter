//! CPU profiler using sampling

use parking_lot::Mutex;
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};
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

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
struct FrameKey {
    function: String,
    file: Option<String>,
    line: Option<u32>,
    column: Option<u32>,
}

#[derive(Debug, Clone)]
struct CpuNode {
    id: u32,
    frame: StackFrame,
    hit_count: u64,
    children: HashMap<FrameKey, u32>,
    child_ids: Vec<u32>,
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

impl CpuProfile {
    /// Export in Chrome/V8 cpuprofile format.
    ///
    /// This format is accepted by Chrome DevTools and Speedscope.
    pub fn to_cpuprofile(&self) -> serde_json::Value {
        let mut nodes = vec![CpuNode {
            id: 1,
            frame: StackFrame {
                function: "(root)".to_string(),
                file: None,
                line: None,
                column: None,
            },
            hit_count: 0,
            children: HashMap::new(),
            child_ids: Vec::new(),
        }];

        let mut sample_node_ids = Vec::with_capacity(self.samples.len());
        let mut time_deltas = Vec::with_capacity(self.samples.len());
        let mut prev_ts = 0u64;

        for sample in &self.samples {
            let mut parent_id = 1u32;
            for frame in &sample.frames {
                let key = FrameKey {
                    function: frame.function.clone(),
                    file: frame.file.clone(),
                    line: frame.line,
                    column: frame.column,
                };

                let parent_index = (parent_id - 1) as usize;
                let child_id = if let Some(existing) = nodes[parent_index].children.get(&key) {
                    *existing
                } else {
                    let new_id = (nodes.len() + 1) as u32;
                    nodes.push(CpuNode {
                        id: new_id,
                        frame: frame.clone(),
                        hit_count: 0,
                        children: HashMap::new(),
                        child_ids: Vec::new(),
                    });
                    nodes[parent_index].children.insert(key, new_id);
                    nodes[parent_index].child_ids.push(new_id);
                    new_id
                };
                parent_id = child_id;
            }

            let leaf_index = (parent_id - 1) as usize;
            nodes[leaf_index].hit_count = nodes[leaf_index].hit_count.saturating_add(1);
            sample_node_ids.push(parent_id);

            let delta = sample.timestamp_us.saturating_sub(prev_ts);
            time_deltas.push(delta);
            prev_ts = sample.timestamp_us;
        }

        let start_time = self.samples.first().map(|s| s.timestamp_us).unwrap_or(0);
        let end_time = self
            .samples
            .last()
            .map(|s| s.timestamp_us)
            .unwrap_or(start_time.saturating_add(self.duration_us));

        let json_nodes: Vec<_> = nodes
            .iter()
            .map(|node| {
                let (url, line, col) = match &node.frame.file {
                    Some(file) => (
                        file.clone(),
                        node.frame
                            .line
                            .map(|v| v.saturating_sub(1) as i64)
                            .unwrap_or(-1),
                        node.frame
                            .column
                            .map(|v| v.saturating_sub(1) as i64)
                            .unwrap_or(-1),
                    ),
                    None => ("".to_string(), -1, -1),
                };

                serde_json::json!({
                    "id": node.id,
                    "callFrame": {
                        "functionName": node.frame.function,
                        "scriptId": "0",
                        "url": url,
                        "lineNumber": line,
                        "columnNumber": col
                    },
                    "hitCount": node.hit_count,
                    "children": node.child_ids
                })
            })
            .collect();

        serde_json::json!({
            "nodes": json_nodes,
            "startTime": start_time,
            "endTime": end_time,
            "samples": sample_node_ids,
            "timeDeltas": time_deltas
        })
    }

    /// Export folded stacks for flamegraph tools.
    ///
    /// Output format: `frame1;frame2;leaf count`
    pub fn to_folded(&self) -> String {
        let mut stacks: BTreeMap<String, u64> = BTreeMap::new();

        for sample in &self.samples {
            if sample.frames.is_empty() {
                continue;
            }

            let stack = sample
                .frames
                .iter()
                .map(|frame| {
                    let mut label = frame.function.replace(';', ":");
                    if let Some(file) = &frame.file {
                        if let Some(line) = frame.line {
                            label.push_str(&format!(" ({}:{})", file, line));
                        } else {
                            label.push_str(&format!(" ({})", file));
                        }
                    }
                    label
                })
                .collect::<Vec<_>>()
                .join(";");

            *stacks.entry(stack).or_insert(0) += 1;
        }

        let mut output = String::new();
        for (stack, count) in stacks {
            output.push_str(&stack);
            output.push(' ');
            output.push_str(&count.to_string());
            output.push('\n');
        }
        output
    }
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
    use serde_json::Value as JsonValue;
    use std::collections::BTreeSet;

    const NODE_CPUPROFILE_FIXTURE: &str = r#"{
      "nodes": [
        {
          "id": 1,
          "callFrame": {
            "functionName": "(root)",
            "scriptId": "0",
            "url": "",
            "lineNumber": -1,
            "columnNumber": -1
          },
          "hitCount": 0,
          "children": [2]
        },
        {
          "id": 2,
          "callFrame": {
            "functionName": "foo",
            "scriptId": "1",
            "url": "file:///tmp/foo.js",
            "lineNumber": 0,
            "columnNumber": 0
          },
          "hitCount": 1,
          "children": []
        }
      ],
      "startTime": 1000,
      "endTime": 2000,
      "samples": [2],
      "timeDeltas": [1000]
    }"#;

    fn keyset(value: &JsonValue) -> BTreeSet<String> {
        value
            .as_object()
            .expect("expected JSON object")
            .keys()
            .cloned()
            .collect()
    }

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

    #[test]
    fn test_cpuprofile_export_shape() {
        let profiler = CpuProfiler::new();
        profiler.start();

        profiler.record_sample(vec![
            StackFrame {
                function: "main".to_string(),
                file: Some("app.js".to_string()),
                line: Some(1),
                column: Some(1),
            },
            StackFrame {
                function: "work".to_string(),
                file: Some("app.js".to_string()),
                line: Some(10),
                column: Some(2),
            },
        ]);

        let profile = profiler.stop();
        let cpuprofile = profile.to_cpuprofile();

        assert!(cpuprofile["nodes"].is_array());
        assert!(cpuprofile["samples"].is_array());
        assert!(cpuprofile["timeDeltas"].is_array());
        assert_eq!(cpuprofile["samples"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn test_cpuprofile_matches_node_fixture_schema() {
        let profiler = CpuProfiler::new();
        profiler.start();
        profiler.record_sample(vec![
            StackFrame {
                function: "main".to_string(),
                file: Some("app.js".to_string()),
                line: Some(1),
                column: Some(1),
            },
            StackFrame {
                function: "work".to_string(),
                file: Some("app.js".to_string()),
                line: Some(2),
                column: Some(1),
            },
        ]);
        let generated = profiler.stop().to_cpuprofile();

        let fixture: JsonValue = serde_json::from_str(NODE_CPUPROFILE_FIXTURE).expect("fixture");
        assert_eq!(keyset(&generated), keyset(&fixture));

        let generated_node = generated["nodes"]
            .as_array()
            .and_then(|nodes| nodes.first())
            .expect("generated nodes[0]");
        let fixture_node = fixture["nodes"]
            .as_array()
            .and_then(|nodes| nodes.first())
            .expect("fixture nodes[0]");
        assert_eq!(keyset(generated_node), keyset(fixture_node));
        assert_eq!(
            keyset(&generated_node["callFrame"]),
            keyset(&fixture_node["callFrame"])
        );

        let sample_len = generated["samples"].as_array().expect("samples").len();
        let deltas_len = generated["timeDeltas"]
            .as_array()
            .expect("timeDeltas")
            .len();
        assert_eq!(sample_len, deltas_len);
        assert!(generated["startTime"].is_number());
        assert!(generated["endTime"].is_number());
    }

    #[test]
    fn test_folded_export_shape() {
        let profiler = CpuProfiler::new();
        profiler.start();

        profiler.record_sample(vec![
            StackFrame {
                function: "main".to_string(),
                file: None,
                line: None,
                column: None,
            },
            StackFrame {
                function: "work".to_string(),
                file: None,
                line: None,
                column: None,
            },
        ]);
        profiler.record_sample(vec![
            StackFrame {
                function: "main".to_string(),
                file: None,
                line: None,
                column: None,
            },
            StackFrame {
                function: "work".to_string(),
                file: None,
                line: None,
                column: None,
            },
        ]);

        let profile = profiler.stop();
        let folded = profile.to_folded();
        assert!(folded.contains("main;work 2"));
    }

    #[test]
    fn test_folded_export_uses_standard_stack_count_lines() {
        let profiler = CpuProfiler::new();
        profiler.start();

        profiler.record_sample(vec![
            StackFrame {
                function: "ma;in".to_string(),
                file: Some("app.js".to_string()),
                line: Some(1),
                column: Some(1),
            },
            StackFrame {
                function: "work".to_string(),
                file: Some("app.js".to_string()),
                line: Some(2),
                column: Some(1),
            },
        ]);

        let folded = profiler.stop().to_folded();
        let mut seen_line = false;
        for line in folded.lines().filter(|line| !line.trim().is_empty()) {
            seen_line = true;
            let mut parts = line.rsplitn(2, ' ');
            let count = parts.next().expect("count");
            let stack = parts.next().expect("stack");
            assert!(
                count.parse::<u64>().is_ok(),
                "invalid folded count in line: {line}"
            );
            assert!(
                !stack.trim().is_empty(),
                "empty folded stack in line: {line}"
            );
            assert!(
                stack.contains(';'),
                "folded stack should preserve frame separators: {line}"
            );
            // Function name semicolons should be escaped to avoid format ambiguity.
            assert!(
                !stack.contains("ma;in"),
                "unescaped semicolon in frame label"
            );
        }
        assert!(seen_line, "expected at least one folded output line");
    }
}
