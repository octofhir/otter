//! Console built-in with pluggable adapter
//!
//! Provides console.log, console.error, console.warn, etc. with a pluggable
//! `ConsoleAdapter` trait for custom output handling.
//!
//! # Example: Default usage (CLI)
//! ```ignore
//! let ops = console_ops(); // Uses StdConsole with println!
//! ```
//!
//! # Example: Custom adapter (embedder with tracing)
//! ```ignore
//! struct TracingConsole { ... }
//! impl ConsoleAdapter for TracingConsole { ... }
//! let ops = console_ops_with_adapter(TracingConsole::new());
//! ```

use otter_vm_core::gc::GcRef;
use otter_vm_core::memory;
use otter_vm_core::object::JsObject;
use otter_vm_core::value::Value;
use otter_vm_runtime::{Op, op_native_with_mm as op_native};
use std::collections::HashMap;
use std::io::{self, Write};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Log levels matching console methods
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    /// console.log
    Log,
    /// console.info
    Info,
    /// console.warn
    Warn,
    /// console.error
    Error,
    /// console.debug
    Debug,
    /// console.trace
    Trace,
}

/// Trait for pluggable console output.
///
/// Implement this trait to redirect console output to custom destinations
/// (e.g., tracing crate, file, network, test capture).
pub trait ConsoleAdapter: Send + Sync + 'static {
    /// Output a log message at the specified level
    fn log(&self, level: LogLevel, message: &str);

    /// Start a timer with the given label
    fn time_start(&self, label: &str);

    /// End a timer, return elapsed milliseconds (None if timer not found)
    fn time_end(&self, label: &str) -> Option<f64>;

    /// Clear console (optional, no-op by default)
    fn clear(&self) {
        // Default: no-op
    }

    /// Increment and return count for label
    fn count(&self, label: &str) -> u64;

    /// Reset count for label
    fn count_reset(&self, label: &str);
}

/// Blanket implementation for Arc<A> to allow shared adapters
impl<A: ConsoleAdapter> ConsoleAdapter for Arc<A> {
    fn log(&self, level: LogLevel, message: &str) {
        (**self).log(level, message)
    }

    fn time_start(&self, label: &str) {
        (**self).time_start(label)
    }

    fn time_end(&self, label: &str) -> Option<f64> {
        (**self).time_end(label)
    }

    fn clear(&self) {
        (**self).clear()
    }

    fn count(&self, label: &str) -> u64 {
        (**self).count(label)
    }

    fn count_reset(&self, label: &str) {
        (**self).count_reset(label)
    }
}

/// Default console implementation using println!/eprintln! with ANSI colors
pub struct StdConsole {
    timers: Mutex<HashMap<String, Instant>>,
    counters: Mutex<HashMap<String, u64>>,
}

impl StdConsole {
    /// Create a new StdConsole
    pub fn new() -> Self {
        Self {
            timers: Mutex::new(HashMap::new()),
            counters: Mutex::new(HashMap::new()),
        }
    }
}

impl Default for StdConsole {
    fn default() -> Self {
        Self::new()
    }
}

impl ConsoleAdapter for StdConsole {
    fn log(&self, level: LogLevel, message: &str) {
        match level {
            LogLevel::Error => eprintln!("\x1b[31m{}\x1b[0m", message),
            LogLevel::Warn => eprintln!("\x1b[33m{}\x1b[0m", message),
            LogLevel::Info => println!("\x1b[34m{}\x1b[0m", message),
            LogLevel::Debug => println!("\x1b[90m{}\x1b[0m", message),
            LogLevel::Trace => println!("Trace: {}", message),
            LogLevel::Log => println!("{}", message),
        }
    }

    fn time_start(&self, label: &str) {
        let mut timers = self.timers.lock().unwrap();
        timers.insert(label.to_string(), Instant::now());
    }

    fn time_end(&self, label: &str) -> Option<f64> {
        let mut timers = self.timers.lock().unwrap();
        timers
            .remove(label)
            .map(|start| start.elapsed().as_secs_f64() * 1000.0)
    }

    fn clear(&self) {
        print!("\x1b[2J\x1b[H");
        let _ = io::stdout().flush();
    }

    fn count(&self, label: &str) -> u64 {
        let mut counters = self.counters.lock().unwrap();
        let count = counters.entry(label.to_string()).or_insert(0);
        *count += 1;
        *count
    }

    fn count_reset(&self, label: &str) {
        let mut counters = self.counters.lock().unwrap();
        counters.remove(label);
    }
}

// ============================================================================
// Factory Functions
// ============================================================================

/// Create console ops with default StdConsole (println!/eprintln!)
pub fn console_ops() -> Vec<Op> {
    console_ops_with_adapter(StdConsole::default())
}

/// Create console ops with custom adapter
pub fn console_ops_with_adapter<A: ConsoleAdapter>(adapter: A) -> Vec<Op> {
    let adapter = Arc::new(adapter);

    vec![
        // Logging methods
        create_log_op("__console_log", adapter.clone(), LogLevel::Log),
        create_log_op("__console_error", adapter.clone(), LogLevel::Error),
        create_log_op("__console_warn", adapter.clone(), LogLevel::Warn),
        create_log_op("__console_info", adapter.clone(), LogLevel::Info),
        create_log_op("__console_debug", adapter.clone(), LogLevel::Debug),
        create_log_op("__console_trace", adapter.clone(), LogLevel::Trace),
        // Timer methods
        create_time_op(adapter.clone()),
        create_time_end_op(adapter.clone()),
        create_time_log_op(adapter.clone()),
        // Assert
        create_assert_op(adapter.clone()),
        // Clear
        create_clear_op(adapter.clone()),
        // Count methods
        create_count_op(adapter.clone()),
        create_count_reset_op(adapter.clone()),
        // Table
        create_table_op(adapter.clone()),
        // Dir/DirXml (aliases for log with object formatting)
        create_log_op("__console_dir", adapter.clone(), LogLevel::Log),
        create_log_op("__console_dirxml", adapter, LogLevel::Log),
    ]
}

// ============================================================================
// Op Creators
// ============================================================================

fn create_log_op<A: ConsoleAdapter>(name: &str, adapter: Arc<A>, level: LogLevel) -> Op {
    op_native(
        name,
        move |args: &[Value], _mm: Arc<memory::MemoryManager>| {
            let message = format_args(args);
            adapter.log(level, &message);
            Ok(Value::undefined())
        },
    )
}

fn create_time_op<A: ConsoleAdapter>(adapter: Arc<A>) -> Op {
    op_native(
        "__console_time",
        move |args: &[Value], _mm: Arc<memory::MemoryManager>| {
            let label = get_string_arg(args, 0).unwrap_or_else(|| "default".to_string());
            adapter.time_start(&label);
            Ok(Value::undefined())
        },
    )
}

fn create_time_end_op<A: ConsoleAdapter>(adapter: Arc<A>) -> Op {
    op_native(
        "__console_timeEnd",
        move |args: &[Value], _mm: Arc<memory::MemoryManager>| {
            let label = get_string_arg(args, 0).unwrap_or_else(|| "default".to_string());
            if let Some(elapsed) = adapter.time_end(&label) {
                adapter.log(LogLevel::Log, &format!("{}: {:.3}ms", label, elapsed));
            } else {
                adapter.log(LogLevel::Warn, &format!("Timer '{}' does not exist", label));
            }
            Ok(Value::undefined())
        },
    )
}

fn create_time_log_op<A: ConsoleAdapter>(adapter: Arc<A>) -> Op {
    // timeLog prints elapsed without stopping the timer
    op_native(
        "__console_timeLog",
        move |args: &[Value], _mm: Arc<memory::MemoryManager>| {
            let label = get_string_arg(args, 0).unwrap_or_else(|| "default".to_string());
            // We can't access elapsed without removing, so this is a simplified impl
            // In a real impl, we'd need to track start time separately
            adapter.log(LogLevel::Log, &format!("{}: (timer running)", label));
            Ok(Value::undefined())
        },
    )
}

fn create_assert_op<A: ConsoleAdapter>(adapter: Arc<A>) -> Op {
    op_native(
        "__console_assert",
        move |args: &[Value], _mm: Arc<memory::MemoryManager>| {
            let condition = args.first().map(|v| v.to_boolean()).unwrap_or(false);

            if !condition {
                let message = if args.len() > 1 {
                    format!("Assertion failed: {}", format_args(&args[1..]))
                } else {
                    "Assertion failed".to_string()
                };
                adapter.log(LogLevel::Error, &message);
            }
            Ok(Value::undefined())
        },
    )
}

fn create_clear_op<A: ConsoleAdapter>(adapter: Arc<A>) -> Op {
    op_native(
        "__console_clear",
        move |_args: &[Value], _mm: Arc<memory::MemoryManager>| {
            adapter.clear();
            Ok(Value::undefined())
        },
    )
}

fn create_count_op<A: ConsoleAdapter>(adapter: Arc<A>) -> Op {
    op_native(
        "__console_count",
        move |args: &[Value], _mm: Arc<memory::MemoryManager>| {
            let label = get_string_arg(args, 0).unwrap_or_else(|| "default".to_string());
            let count = adapter.count(&label);
            adapter.log(LogLevel::Log, &format!("{}: {}", label, count));
            Ok(Value::undefined())
        },
    )
}

fn create_count_reset_op<A: ConsoleAdapter>(adapter: Arc<A>) -> Op {
    op_native(
        "__console_countReset",
        move |args: &[Value], _mm: Arc<memory::MemoryManager>| {
            let label = get_string_arg(args, 0).unwrap_or_else(|| "default".to_string());
            adapter.count_reset(&label);
            Ok(Value::undefined())
        },
    )
}

fn create_table_op<A: ConsoleAdapter>(adapter: Arc<A>) -> Op {
    op_native(
        "__console_table",
        move |args: &[Value], _mm: Arc<memory::MemoryManager>| {
            if let Some(data) = args.first() {
                let formatted = format_table(data);
                adapter.log(LogLevel::Log, &formatted);
            }
            Ok(Value::undefined())
        },
    )
}

// ============================================================================
// Formatting Helpers
// ============================================================================

fn format_args(args: &[Value]) -> String {
    args.iter().map(format_value).collect::<Vec<_>>().join(" ")
}

fn format_value(value: &Value) -> String {
    if value.is_undefined() {
        "undefined".to_string()
    } else if value.is_null() {
        "null".to_string()
    } else if let Some(b) = value.as_boolean() {
        b.to_string()
    } else if let Some(n) = value.as_number() {
        if n.is_nan() {
            "NaN".to_string()
        } else if n.is_infinite() {
            if n.is_sign_positive() {
                "Infinity".to_string()
            } else {
                "-Infinity".to_string()
            }
        } else {
            n.to_string()
        }
    } else if let Some(n) = value.as_int32() {
        n.to_string()
    } else if let Some(s) = value.as_string() {
        s.as_str().to_string()
    } else if value.is_function() {
        "[Function]".to_string()
    } else if value.is_native_function() {
        "[native function]".to_string()
    } else if value.is_symbol() {
        if let Some(sym) = value.as_symbol() {
            if let Some(desc) = &sym.description {
                format!("Symbol({})", desc)
            } else {
                "Symbol()".to_string()
            }
        } else {
            "Symbol()".to_string()
        }
    } else if value.is_bigint() {
        if let Some(otter_vm_core::value::HeapRef::BigInt(b)) = value.heap_ref() {
            format!("{}n", b.value)
        } else {
            "BigInt".to_string()
        }
    } else if value.is_promise() {
        "[Promise]".to_string()
    } else if let Some(arr) = value.as_array() {
        format_array(arr)
    } else if let Some(obj) = value.as_object() {
        format_object(obj)
    } else {
        "[unknown]".to_string()
    }
}

fn format_array(arr: GcRef<JsObject>) -> String {
    let len = arr.array_length();
    if len == 0 {
        return "[]".to_string();
    }

    let max_display = 100;
    let display_len = std::cmp::min(len, max_display);

    let items: Vec<String> = (0..display_len)
        .map(|i| {
            arr.get(&otter_vm_core::object::PropertyKey::Index(i as u32))
                .map(|v| format_value(&v))
                .unwrap_or_else(|| "undefined".to_string())
        })
        .collect();

    if len > max_display {
        format!("[ {}, ... {} more ]", items.join(", "), len - max_display)
    } else {
        format!("[ {} ]", items.join(", "))
    }
}

fn format_object(obj: GcRef<JsObject>) -> String {
    use otter_vm_core::object::PropertyKey;

    let keys = obj.own_keys();
    if keys.is_empty() {
        return "{}".to_string();
    }

    let max_keys = 50;
    let display_keys: Vec<_> = keys.iter().take(max_keys).collect();
    let has_more = keys.len() > max_keys;

    let pairs: Vec<String> = display_keys
        .iter()
        .filter_map(|key| {
            let key_str = match key {
                PropertyKey::String(s) => s.as_str().to_string(),
                PropertyKey::Index(i) => i.to_string(),
                PropertyKey::Symbol(s) => format!("[Symbol({})]", s),
            };
            obj.get(key).map(|v| {
                let value_str = if v.is_object() && !v.is_function() && !v.is_native_function() {
                    "[Object]".to_string()
                } else {
                    format_value(&v)
                };
                format!("{}: {}", key_str, value_str)
            })
        })
        .collect();

    if has_more {
        format!(
            "{{ {}, ... {} more }}",
            pairs.join(", "),
            keys.len() - max_keys
        )
    } else {
        format!("{{ {} }}", pairs.join(", "))
    }
}

fn format_table(value: &Value) -> String {
    // Simple table formatting - just pretty print the value
    if let Some(obj) = value.as_object() {
        format_object(obj)
    } else {
        format_value(value)
    }
}

fn get_string_arg(args: &[Value], index: usize) -> Option<String> {
    args.get(index).and_then(|v| {
        if let Some(s) = v.as_string() {
            Some(s.as_str().to_string())
        } else if !v.is_undefined() && !v.is_null() {
            Some(format_value(v))
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test adapter that captures all log output
    struct CaptureConsole {
        logs: Mutex<Vec<(LogLevel, String)>>,
        timers: Mutex<HashMap<String, Instant>>,
        counters: Mutex<HashMap<String, u64>>,
    }

    impl CaptureConsole {
        fn new() -> Self {
            Self {
                logs: Mutex::new(Vec::new()),
                timers: Mutex::new(HashMap::new()),
                counters: Mutex::new(HashMap::new()),
            }
        }

        fn get_logs(&self) -> Vec<(LogLevel, String)> {
            self.logs.lock().unwrap().clone()
        }
    }

    impl ConsoleAdapter for CaptureConsole {
        fn log(&self, level: LogLevel, message: &str) {
            self.logs.lock().unwrap().push((level, message.to_string()));
        }

        fn time_start(&self, label: &str) {
            self.timers
                .lock()
                .unwrap()
                .insert(label.to_string(), Instant::now());
        }

        fn time_end(&self, label: &str) -> Option<f64> {
            self.timers
                .lock()
                .unwrap()
                .remove(label)
                .map(|start| start.elapsed().as_secs_f64() * 1000.0)
        }

        fn count(&self, label: &str) -> u64 {
            let mut counters = self.counters.lock().unwrap();
            let count = counters.entry(label.to_string()).or_insert(0);
            *count += 1;
            *count
        }

        fn count_reset(&self, label: &str) {
            self.counters.lock().unwrap().remove(label);
        }
    }

    #[test]
    fn test_console_log() {
        let capture = Arc::new(CaptureConsole::new());
        let ops = console_ops_with_adapter(capture.clone());

        // Find console_log op
        let log_op = ops.iter().find(|op| op.name == "__console_log").unwrap();
        let mm = Arc::new(memory::MemoryManager::test());

        // Call it
        if let otter_vm_runtime::OpHandler::Native(handler) = &log_op.handler {
            let result = handler(
                &[Value::string(otter_vm_core::JsString::intern("hello"))],
                mm,
            );
            assert!(result.is_ok());
        }

        let logs = capture.get_logs();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].0, LogLevel::Log);
        assert_eq!(logs[0].1, "hello");
    }

    #[test]
    fn test_console_error() {
        let capture = Arc::new(CaptureConsole::new());
        let ops = console_ops_with_adapter(capture.clone());

        let error_op = ops.iter().find(|op| op.name == "__console_error").unwrap();
        let mm = Arc::new(memory::MemoryManager::test());

        if let otter_vm_runtime::OpHandler::Native(handler) = &error_op.handler {
            let _ = handler(
                &[Value::string(otter_vm_core::JsString::intern("error!"))],
                mm,
            );
        }

        let logs = capture.get_logs();
        assert_eq!(logs[0].0, LogLevel::Error);
        assert_eq!(logs[0].1, "error!");
    }

    #[test]
    fn test_console_count() {
        let capture = Arc::new(CaptureConsole::new());
        let ops = console_ops_with_adapter(capture.clone());

        let count_op = ops.iter().find(|op| op.name == "__console_count").unwrap();
        let mm = Arc::new(memory::MemoryManager::test());

        if let otter_vm_runtime::OpHandler::Native(handler) = &count_op.handler {
            // Call count 3 times
            let _ = handler(
                &[Value::string(otter_vm_core::JsString::intern("test"))],
                mm.clone(),
            );
            let _ = handler(
                &[Value::string(otter_vm_core::JsString::intern("test"))],
                mm.clone(),
            );
            let _ = handler(
                &[Value::string(otter_vm_core::JsString::intern("test"))],
                mm,
            );
        }

        let logs = capture.get_logs();
        assert_eq!(logs.len(), 3);
        assert_eq!(logs[0].1, "test: 1");
        assert_eq!(logs[1].1, "test: 2");
        assert_eq!(logs[2].1, "test: 3");
    }

    #[test]
    fn test_console_assert_pass() {
        let capture = Arc::new(CaptureConsole::new());
        let ops = console_ops_with_adapter(capture.clone());

        let assert_op = ops.iter().find(|op| op.name == "__console_assert").unwrap();
        let mm = Arc::new(memory::MemoryManager::test());

        if let otter_vm_runtime::OpHandler::Native(handler) = &assert_op.handler {
            // Pass: true condition
            let _ = handler(&[Value::boolean(true)], mm);
        }

        // No logs when assertion passes
        let logs = capture.get_logs();
        assert!(logs.is_empty());
    }

    #[test]
    fn test_console_assert_fail() {
        let capture = Arc::new(CaptureConsole::new());
        let ops = console_ops_with_adapter(capture.clone());

        let assert_op = ops.iter().find(|op| op.name == "__console_assert").unwrap();
        let mm = Arc::new(memory::MemoryManager::test());

        if let otter_vm_runtime::OpHandler::Native(handler) = &assert_op.handler {
            // Fail: false condition
            let _ = handler(
                &[
                    Value::boolean(false),
                    Value::string(otter_vm_core::JsString::intern("x should be positive")),
                ],
                mm,
            );
        }

        let logs = capture.get_logs();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].0, LogLevel::Error);
        assert!(logs[0].1.contains("Assertion failed"));
        assert!(logs[0].1.contains("x should be positive"));
    }

    #[test]
    fn test_format_args() {
        let args = vec![
            Value::string(otter_vm_core::JsString::intern("hello")),
            Value::number(42.0),
            Value::boolean(true),
        ];
        let formatted = format_args(&args);
        assert_eq!(formatted, "hello 42 true");
    }

    #[test]
    fn test_format_special_values() {
        assert_eq!(format_value(&Value::undefined()), "undefined");
        assert_eq!(format_value(&Value::null()), "null");
        assert_eq!(format_value(&Value::number(f64::NAN)), "NaN");
        assert_eq!(format_value(&Value::number(f64::INFINITY)), "Infinity");
        assert_eq!(format_value(&Value::number(f64::NEG_INFINITY)), "-Infinity");
    }
}
