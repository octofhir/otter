// SPDX-License-Identifier: MIT OR Apache-2.0
//! Trace and dump system for VM debugging.
//!
//! Provides configurable instruction-level tracing with ring buffers,
//! snapshots, and formatted output for debugging test262 timeouts and failures.

use std::path::PathBuf;

/// Top-level schema version for Chrome trace JSON emitted by VM instruction tracing.
pub const TRACE_EVENT_SCHEMA_VERSION: u32 = 1;

/// Configuration for trace capture
#[derive(Debug, Clone)]
pub struct TraceConfig {
    /// Whether tracing is enabled
    pub enabled: bool,
    /// Trace capture mode
    pub mode: TraceMode,
    /// Size of ring buffer for recent instructions
    pub ring_buffer_size: usize,
    /// Optional path for trace output
    pub output_path: Option<PathBuf>,
    /// Filter pattern for module/function names (regex)
    pub filter: Option<String>,
    /// Capture timing information (adds overhead)
    pub capture_timing: bool,
}

impl Default for TraceConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            mode: TraceMode::RingBuffer,
            ring_buffer_size: 100,
            output_path: None,
            filter: None,
            capture_timing: false,
        }
    }
}

/// Trace capture mode
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TraceMode {
    /// Update snapshot only (minimal overhead)
    Snapshot,
    /// Capture last N instructions in ring buffer (MVP)
    RingBuffer,
    /// Capture every instruction (Phase 2 - not yet implemented)
    FullTrace,
}

/// Single trace entry capturing instruction execution state
#[derive(Debug, Clone)]
pub struct TraceEntry {
    /// Sequential instruction number
    pub instruction_number: u64,
    /// Program counter (bytecode offset)
    pub pc: usize,
    /// Function index being executed
    pub function_index: u32,
    /// Function name (if available)
    pub function_name: Option<String>,
    /// Module URL
    pub module_url: String,
    /// Opcode mnemonic
    pub opcode: String,
    /// Formatted operands
    pub operands: String,
    /// Registers modified by this instruction (register index, value string)
    pub modified_registers: Vec<(u16, String)>,
    /// Execution time in nanoseconds (if timing is enabled)
    pub execution_time_ns: Option<u64>,
}

/// Ring buffer for storing recent trace entries
pub struct TraceRingBuffer {
    entries: Vec<TraceEntry>,
    capacity: usize,
    head: usize,
    full: bool,
}

impl TraceRingBuffer {
    /// Create new ring buffer with given capacity
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: Vec::with_capacity(capacity),
            capacity,
            head: 0,
            full: false,
        }
    }

    /// Push new entry into ring buffer
    pub fn push(&mut self, entry: TraceEntry) {
        if self.full {
            self.entries[self.head] = entry;
            self.head = (self.head + 1) % self.capacity;
        } else {
            self.entries.push(entry);
            if self.entries.len() == self.capacity {
                self.full = true;
            }
        }
    }

    /// Iterate over entries in chronological order (oldest first)
    pub fn iter(&self) -> impl Iterator<Item = &TraceEntry> {
        if self.full {
            // If buffer is full, start from head (oldest entry)
            self.entries[self.head..]
                .iter()
                .chain(self.entries[..self.head].iter())
        } else {
            // If not full, iterate from start
            self.entries.iter().chain([].iter())
        }
    }

    /// Get number of entries currently stored
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Check if buffer is empty
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Get capacity of buffer
    pub fn capacity(&self) -> usize {
        self.capacity
    }
}

/// Trace writer for full trace mode
pub struct TraceWriter {
    file: std::io::BufWriter<std::fs::File>,
    format: TraceOutputFormat,
    first_event: bool,
    closed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TraceOutputFormat {
    Text,
    ChromeTraceJson,
}

impl TraceWriter {
    /// Create new trace writer
    pub fn new(path: &PathBuf) -> std::io::Result<Self> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(path)?;

        let mut writer = Self {
            file: std::io::BufWriter::new(file),
            format: if path
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
            {
                TraceOutputFormat::ChromeTraceJson
            } else {
                TraceOutputFormat::Text
            },
            first_event: true,
            closed: false,
        };

        use std::io::Write;

        match writer.format {
            TraceOutputFormat::Text => {
                writeln!(
                    writer.file,
                    "════════════════════════════════════════════════════════════════════════════════"
                )?;
                writeln!(writer.file, "Otter VM Execution Trace")?;
                writeln!(
                    writer.file,
                    "════════════════════════════════════════════════════════════════════════════════"
                )?;
                writeln!(writer.file)?;
                writeln!(
                    writer.file,
                    "{:>8}  {:>6}  {:>4}  {:<25}  {:<15}  {}",
                    "INST#", "PC", "FN", "MODULE", "OPCODE", "OPERANDS"
                )?;
                writeln!(
                    writer.file,
                    "────────────────────────────────────────────────────────────────────────────────"
                )?;
            }
            TraceOutputFormat::ChromeTraceJson => {
                write!(
                    writer.file,
                    r#"{{"otterTraceSchemaVersion":{},"displayTimeUnit":"ns","traceEvents":["#,
                    TRACE_EVENT_SCHEMA_VERSION
                )?;
            }
        }

        writer.file.flush()?;

        Ok(writer)
    }

    /// Write trace entry
    pub fn write_entry(&mut self, entry: &TraceEntry) -> std::io::Result<()> {
        use std::io::Write;

        match self.format {
            TraceOutputFormat::Text => {
                let module_short = if entry.module_url.len() > 25 {
                    format!("...{}", &entry.module_url[entry.module_url.len() - 22..])
                } else {
                    entry.module_url.clone()
                };

                let function_name = entry.function_name.as_deref().unwrap_or("<anon>");
                let function_short = if function_name.len() > 4 {
                    &function_name[..4]
                } else {
                    function_name
                };

                write!(
                    self.file,
                    "{:>8}  {:>6x}  {:>4}  {:<25}  {:<15}  {}",
                    entry.instruction_number,
                    entry.pc,
                    function_short,
                    module_short,
                    entry.opcode,
                    entry.operands
                )?;

                if let Some(time_ns) = entry.execution_time_ns {
                    write!(self.file, "  [{:.2}µs]", time_ns as f64 / 1000.0)?;
                }

                writeln!(self.file)?;
            }
            TraceOutputFormat::ChromeTraceJson => {
                if !self.first_event {
                    self.file.write_all(b",")?;
                } else {
                    self.first_event = false;
                }

                let event = serde_json::json!({
                    "name": entry.opcode,
                    "cat": "vm.instruction",
                    "ph": "X",
                    "ts": entry.instruction_number,
                    "dur": entry.execution_time_ns.unwrap_or(0),
                    "pid": 1,
                    "tid": 1,
                    "args": {
                        "module": entry.module_url,
                        "function": entry.function_name,
                        "pc": entry.pc,
                        "function_index": entry.function_index,
                        "operands": entry.operands,
                        "modified_registers": entry.modified_registers,
                    }
                });

                serde_json::to_writer(&mut self.file, &event)?;
            }
        }

        // Flush every 100 entries to avoid losing data on crash
        if entry.instruction_number % 100 == 0 {
            self.file.flush()?;
        }

        Ok(())
    }

    /// Flush and close
    pub fn close(mut self) -> std::io::Result<()> {
        self.finish()
    }

    fn finish(&mut self) -> std::io::Result<()> {
        if self.closed {
            return Ok(());
        }

        use std::io::Write;
        match self.format {
            TraceOutputFormat::Text => {
                writeln!(self.file)?;
                writeln!(
                    self.file,
                    "════════════════════════════════════════════════════════════════════════════════"
                )?;
            }
            TraceOutputFormat::ChromeTraceJson => {
                self.file.write_all(b"]}\n")?;
            }
        }

        self.closed = true;
        self.file.flush()
    }
}

impl Drop for TraceWriter {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

/// Trace state maintained by VM context
pub struct TraceState {
    /// Trace configuration
    pub config: TraceConfig,
    /// Ring buffer of recent instructions
    pub ring_buffer: TraceRingBuffer,
    /// Total instruction counter
    pub instruction_counter: u64,
    /// Trace writer (for full trace mode)
    pub trace_writer: Option<TraceWriter>,
    /// Compiled filter regex
    pub filter_regex: Option<regex::Regex>,
}

impl TraceState {
    /// Create new trace state with given config
    pub fn new(config: TraceConfig) -> Self {
        let ring_buffer_size = config.ring_buffer_size;

        // Create trace writer for full trace mode
        let trace_writer = if config.mode == TraceMode::FullTrace {
            if let Some(ref path) = config.output_path {
                match TraceWriter::new(path) {
                    Ok(writer) => Some(writer),
                    Err(e) => {
                        eprintln!("Failed to create trace writer: {}", e);
                        None
                    }
                }
            } else {
                eprintln!("FullTrace mode requires output_path");
                None
            }
        } else {
            None
        };

        // Compile filter regex if provided
        let filter_regex =
            config
                .filter
                .as_ref()
                .and_then(|pattern| match regex::Regex::new(pattern) {
                    Ok(re) => Some(re),
                    Err(e) => {
                        eprintln!("Invalid trace filter regex: {}", e);
                        None
                    }
                });

        Self {
            config,
            ring_buffer: TraceRingBuffer::new(ring_buffer_size),
            instruction_counter: 0,
            trace_writer,
            filter_regex,
        }
    }

    /// Check if entry matches filter
    pub fn matches_filter(&self, entry: &TraceEntry) -> bool {
        if let Some(ref regex) = self.filter_regex {
            // Match against module URL or function name
            if let Some(ref fname) = entry.function_name {
                if regex.is_match(fname) {
                    return true;
                }
            }
            regex.is_match(&entry.module_url)
        } else {
            true // No filter = match all
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_trace_entry(instruction_number: u64) -> TraceEntry {
        TraceEntry {
            instruction_number,
            pc: instruction_number as usize,
            function_index: 0,
            function_name: Some("main".to_string()),
            module_url: "test.js".to_string(),
            opcode: "LoadInt32".to_string(),
            operands: format!("LoadInt32 {{ dst: 0, value: {instruction_number} }}"),
            modified_registers: vec![(0, instruction_number.to_string())],
            execution_time_ns: Some(instruction_number),
        }
    }

    #[test]
    fn test_trace_writer_writes_chrome_trace_json_when_path_is_json() {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "otter-trace-test-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time should be monotonic")
                .as_nanos()
        ));

        {
            let mut writer = TraceWriter::new(&path).expect("writer should be created");
            writer
                .write_entry(&TraceEntry {
                    operands: "LoadInt32 { dst: 0, value: 42 }".to_string(),
                    execution_time_ns: Some(123),
                    ..make_trace_entry(1)
                })
                .expect("entry should be written");
        } // drop => close JSON object

        let contents = std::fs::read_to_string(&path).expect("trace file should be readable");
        let json: serde_json::Value =
            serde_json::from_str(&contents).expect("trace file should be valid json");
        assert_eq!(
            json["otterTraceSchemaVersion"],
            serde_json::json!(TRACE_EVENT_SCHEMA_VERSION)
        );
        let events = json["traceEvents"]
            .as_array()
            .expect("traceEvents should be an array");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["name"], "LoadInt32");

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn test_ring_buffer_overflow_keeps_latest_entries_in_chronological_order() {
        let mut ring = TraceRingBuffer::new(3);
        for instruction in 1..=10 {
            ring.push(make_trace_entry(instruction));
        }

        let retained: Vec<u64> = ring.iter().map(|entry| entry.instruction_number).collect();
        assert_eq!(retained, vec![8, 9, 10]);
    }

    #[test]
    fn test_filter_stress_matches_expected_entries() {
        let state = TraceState::new(TraceConfig {
            enabled: true,
            mode: TraceMode::RingBuffer,
            ring_buffer_size: 256,
            output_path: None,
            filter: Some(r"(module-hot|hot_fn)".to_string()),
            capture_timing: false,
        });

        let mut matched = 0usize;
        for idx in 0..20_000 {
            let module_url = if idx % 10 == 0 {
                "module-hot.js".to_string()
            } else {
                format!("module-{idx}.js")
            };
            let function_name = if idx % 13 == 0 {
                Some("hot_fn".to_string())
            } else {
                Some("cold_fn".to_string())
            };
            let entry = TraceEntry {
                module_url,
                function_name,
                ..make_trace_entry(idx as u64 + 1)
            };
            if state.matches_filter(&entry) {
                matched += 1;
            }
        }

        assert!(matched > 0, "expected hot module/function matches");
        assert!(matched < 20_000, "filter should not match every entry");
    }
}
