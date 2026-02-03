// SPDX-License-Identifier: MIT OR Apache-2.0
//! Trace and dump system for VM debugging.
//!
//! Provides configurable instruction-level tracing with ring buffers,
//! snapshots, and formatted output for debugging test262 timeouts and failures.

use std::path::PathBuf;

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
        };

        // Write header
        use std::io::Write;
        writeln!(writer.file, "════════════════════════════════════════════════════════════════════════════════")?;
        writeln!(writer.file, "Otter VM Execution Trace")?;
        writeln!(writer.file, "════════════════════════════════════════════════════════════════════════════════")?;
        writeln!(writer.file)?;
        writeln!(writer.file, "{:>8}  {:>6}  {:>4}  {:<25}  {:<15}  {}",
                 "INST#", "PC", "FN", "MODULE", "OPCODE", "OPERANDS")?;
        writeln!(writer.file, "────────────────────────────────────────────────────────────────────────────────")?;
        writer.file.flush()?;

        Ok(writer)
    }

    /// Write trace entry
    pub fn write_entry(&mut self, entry: &TraceEntry) -> std::io::Result<()> {
        use std::io::Write;

        let module_short = if entry.module_url.len() > 25 {
            format!("...{}", &entry.module_url[entry.module_url.len()-22..])
        } else {
            entry.module_url.clone()
        };

        let function_name = entry.function_name.as_deref().unwrap_or("<anon>");
        let function_short = if function_name.len() > 4 {
            &function_name[..4]
        } else {
            function_name
        };

        write!(self.file, "{:>8}  {:>6x}  {:>4}  {:<25}  {:<15}  {}",
               entry.instruction_number,
               entry.pc,
               function_short,
               module_short,
               entry.opcode,
               entry.operands)?;

        if let Some(time_ns) = entry.execution_time_ns {
            write!(self.file, "  [{:.2}µs]", time_ns as f64 / 1000.0)?;
        }

        writeln!(self.file)?;

        // Flush every 100 entries to avoid losing data on crash
        if entry.instruction_number % 100 == 0 {
            self.file.flush()?;
        }

        Ok(())
    }

    /// Flush and close
    pub fn close(mut self) -> std::io::Result<()> {
        use std::io::Write;
        writeln!(self.file)?;
        writeln!(self.file, "════════════════════════════════════════════════════════════════════════════════")?;
        self.file.flush()
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
        let filter_regex = config.filter.as_ref().and_then(|pattern| {
            match regex::Regex::new(pattern) {
                Ok(re) => Some(re),
                Err(e) => {
                    eprintln!("Invalid trace filter regex: {}", e);
                    None
                }
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
