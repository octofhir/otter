//! Memory usage helpers for JavaScriptCore.

use crate::bindings::{JSContextRef, OtterJscHeapStats, otter_jsc_heap_stats};

#[derive(Clone, Copy, Debug, Default)]
pub struct JscHeapStats {
    pub heap_size: u64,
    pub heap_capacity: u64,
    pub extra_memory: u64,
    pub array_buffer: u64,
}

pub fn jsc_heap_stats(ctx: JSContextRef) -> Option<JscHeapStats> {
    let mut stats = OtterJscHeapStats::default();
    let ok = unsafe { otter_jsc_heap_stats(ctx, &mut stats) };
    if !ok {
        return None;
    }

    Some(JscHeapStats {
        heap_size: stats.heap_size as u64,
        heap_capacity: stats.heap_capacity as u64,
        extra_memory: stats.extra_memory as u64,
        array_buffer: stats.array_buffer as u64,
    })
}
