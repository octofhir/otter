//! Process extension module using the new architecture.
//!
//! This module provides the process extension for process-related helpers.
//!
//! ## Architecture
//!
//! - `process.rs` - Rust process info implementation
//! - `process_ext.rs` - Extension creation with ops

use otter_runtime::extension::{op_sync, RuntimeContextHandle};
use otter_runtime::memory::jsc_heap_stats;
use otter_runtime::Extension;
use serde_json::json;

#[derive(Default)]
struct MemoryUsage {
    rss: u64,
    heap_total: u64,
    heap_used: u64,
    external: u64,
    array_buffers: u64,
}

#[cfg(target_os = "linux")]
fn memory_usage() -> MemoryUsage {
    let statm = std::fs::read_to_string("/proc/self/statm").ok();
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64;
    if let Some(contents) = statm {
        let mut parts = contents.split_whitespace();
        let size = parts.next().and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
        let rss = parts.next().and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
        let rss_bytes = rss * page_size;
        let size_bytes = size * page_size;
        return MemoryUsage {
            rss: rss_bytes,
            heap_total: size_bytes,
            heap_used: rss_bytes,
            external: 0,
            array_buffers: 0,
        };
    }
    MemoryUsage::default()
}

#[cfg(all(unix, not(target_os = "linux")))]
fn memory_usage() -> MemoryUsage {
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    let result = unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) };
    if result == 0 {
        let rss = usage.ru_maxrss as u64;
        return MemoryUsage {
            rss,
            heap_total: rss,
            heap_used: rss,
            external: 0,
            array_buffers: 0,
        };
    }
    MemoryUsage::default()
}

#[cfg(not(unix))]
fn memory_usage() -> MemoryUsage {
    MemoryUsage::default()
}

fn memory_usage_json(ctx: Option<RuntimeContextHandle>) -> serde_json::Value {
    let mut usage = memory_usage();
    if let Some(handle) = ctx {
        if let Some(stats) = jsc_heap_stats(handle.ctx()) {
            usage.heap_total = stats.heap_capacity;
            usage.heap_used = stats.heap_size;
            usage.external = stats.extra_memory;
            usage.array_buffers = stats.array_buffer;
        }
    }
    json!({
        "rss": usage.rss,
        "heapTotal": usage.heap_total,
        "heapUsed": usage.heap_used,
        "external": usage.external,
        "arrayBuffers": usage.array_buffers,
    })
}

/// Create the process extension.
///
/// This extension provides process-related helpers like memory usage.
pub fn extension() -> Extension {
    Extension::new("process").with_ops(vec![op_sync(
        "__otter_process_memory_usage",
        |ctx, _args| {
            let handle = ctx.state().get::<RuntimeContextHandle>().map(|h| *h);
            Ok(memory_usage_json(handle))
        },
    )])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extension_creation() {
        let ext = extension();
        assert_eq!(ext.name(), "process");
    }

    #[test]
    fn test_memory_usage() {
        let usage = memory_usage();
        // Just check it doesn't panic
        assert!(usage.rss >= 0);
    }
}
