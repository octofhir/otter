//! Code cache — stores compiled functions with tier segmentation and aging.
//!
//! The cache is thread-local (single-threaded VM). Functions are keyed
//! by their raw pointer address, which is stable for the function's lifetime.
//!
//! ## Segmentation (HotSpot-inspired)
//!
//! Baseline (Tier 1) and optimized (Tier 2) code are tracked separately.
//! This prevents short-lived speculative code from polluting hot code locality.
//!
//! ## Aging
//!
//! Each entry tracks: call count, deopt count, last-use timestamp.
//! Cold and unstable code is flushed first when memory pressure hits.
//!
//! Spec: Phase 7.2 of JIT_INCREMENTAL_PLAN.md

use std::cell::RefCell;
use std::collections::HashMap;

use crate::Tier;
use crate::code_memory::{CompiledCodeOrigin, CompiledFunction};
use crate::telemetry::CodeCacheStats;

/// Key for the code cache: raw pointer to the bytecode Function.
type CacheKey = usize;

/// Metadata tracked per cached entry.
struct CacheEntry {
    compiled: CompiledFunction,
    tier: Tier,
    /// Number of times JIT code was entered for this function.
    call_count: u64,
    /// Number of deopts since this version was compiled.
    deopt_count: u32,
    /// Monotonic timestamp of last use (incremented on each cache hit).
    last_use: u64,
}

/// Thread-local code cache with tier segmentation.
struct CodeCache {
    entries: HashMap<CacheKey, CacheEntry>,
    /// Monotonic clock for aging.
    clock: u64,
    /// Total code bytes across all entries.
    total_bytes: usize,
}

impl CodeCache {
    fn new() -> Self {
        Self {
            entries: HashMap::new(),
            clock: 0,
            total_bytes: 0,
        }
    }

    fn tick(&mut self) -> u64 {
        self.clock += 1;
        self.clock
    }
}

thread_local! {
    static CACHE: RefCell<CodeCache> = RefCell::new(CodeCache::new());
}

/// Look up a compiled function by its bytecode Function pointer.
pub fn get(func_ptr: *const otter_vm::Function) -> Option<*const u8> {
    CACHE.with(|cache| {
        let mut c = cache.borrow_mut();
        let ts = c.tick();
        c.entries.get_mut(&(func_ptr as usize)).map(|entry| {
            entry.call_count += 1;
            entry.last_use = ts;
            entry.compiled.entry
        })
    })
}

/// Look up the OSR-entry native offset for `func_ptr` at `byte_pc`.
///
/// Returns `Some(native_offset)` if the compiled function has an OSR
/// trampoline targeting `byte_pc` (i.e. the bytecode PC is a recognised
/// loop header whose first body op passes the OSR safety filter).
/// Returns `None` if the function isn't compiled or has no trampoline
/// for the given PC. Bumps the entry's `last_use` clock so OSR hits
/// keep the function alive in the cache.
pub fn osr_native_offset(func_ptr: *const otter_vm::Function, byte_pc: u32) -> Option<u32> {
    CACHE.with(|cache| {
        let mut c = cache.borrow_mut();
        let ts = c.tick();
        c.entries.get_mut(&(func_ptr as usize)).and_then(|entry| {
            let lookup = entry
                .compiled
                .osr_entries
                .binary_search_by_key(&byte_pc, |(pc, _)| *pc)
                .ok()
                .map(|idx| entry.compiled.osr_entries[idx].1);
            if lookup.is_some() {
                entry.call_count += 1;
                entry.last_use = ts;
            }
            lookup
        })
    })
}

/// Store a compiled function in the cache.
pub fn insert(func_ptr: *const otter_vm::Function, compiled: CompiledFunction) {
    insert_with_tier(func_ptr, compiled, Tier::Baseline);
}

/// Store a compiled function with explicit tier.
pub fn insert_with_tier(
    func_ptr: *const otter_vm::Function,
    compiled: CompiledFunction,
    tier: Tier,
) {
    CACHE.with(|cache| {
        let mut c = cache.borrow_mut();
        let ts = c.tick();
        let code_size = compiled.code_size;

        // Remove old entry's size if replacing.
        if let Some(old) = c.entries.get(&(func_ptr as usize)) {
            c.total_bytes = c.total_bytes.saturating_sub(old.compiled.code_size);
        }

        c.total_bytes += code_size;
        c.entries.insert(
            func_ptr as usize,
            CacheEntry {
                compiled,
                tier,
                call_count: 0,
                deopt_count: 0,
                last_use: ts,
            },
        );
    });
}

/// Record a deopt for a cached function.
pub fn record_deopt(func_ptr: *const otter_vm::Function) {
    CACHE.with(|cache| {
        if let Some(entry) = cache.borrow_mut().entries.get_mut(&(func_ptr as usize)) {
            entry.deopt_count += 1;
        }
    });
}

/// Check if a function is already compiled.
pub fn contains(func_ptr: *const otter_vm::Function) -> bool {
    CACHE.with(|cache| cache.borrow().entries.contains_key(&(func_ptr as usize)))
}

/// Get the tier of a cached function.
pub fn tier_of(func_ptr: *const otter_vm::Function) -> Option<Tier> {
    CACHE.with(|cache| {
        cache
            .borrow()
            .entries
            .get(&(func_ptr as usize))
            .map(|e| e.tier)
    })
}

/// Get the code origin of a cached function.
#[must_use]
pub fn origin_of(func_ptr: *const otter_vm::Function) -> Option<CompiledCodeOrigin> {
    CACHE.with(|cache| {
        cache
            .borrow()
            .entries
            .get(&(func_ptr as usize))
            .map(|e| e.compiled.origin)
    })
}

/// Number of compiled functions in the cache.
pub fn len() -> usize {
    CACHE.with(|cache| cache.borrow().entries.len())
}

/// Clear the entire cache (e.g., on runtime teardown).
pub fn clear() {
    CACHE.with(|cache| {
        let mut c = cache.borrow_mut();
        c.entries.clear();
        c.total_bytes = 0;
    });
}

/// Remove a single function from the cache.
pub fn invalidate(func_ptr: *const otter_vm::Function) {
    CACHE.with(|cache| {
        let mut c = cache.borrow_mut();
        if let Some(entry) = c.entries.remove(&(func_ptr as usize)) {
            c.total_bytes = c.total_bytes.saturating_sub(entry.compiled.code_size);
        }
    });
}

/// Get code cache statistics (for telemetry).
pub fn stats() -> CodeCacheStats {
    CACHE.with(|cache| {
        let c = cache.borrow();
        let mut s = CodeCacheStats {
            function_count: c.entries.len(),
            total_bytes: c.total_bytes,
            ..CodeCacheStats::default()
        };
        for entry in c.entries.values() {
            match entry.tier {
                Tier::Baseline => {
                    s.tier1_count += 1;
                    s.tier1_bytes += entry.compiled.code_size;
                }
                Tier::Optimized => {
                    s.tier2_count += 1;
                    s.tier2_bytes += entry.compiled.code_size;
                }
            }
        }
        s
    })
}

/// Flush cold code entries to free memory.
///
/// Removes entries that haven't been used recently and have low call counts.
/// Returns the number of entries flushed.
pub fn flush_cold(max_age: u64) -> usize {
    CACHE.with(|cache| {
        let mut c = cache.borrow_mut();
        let current_clock = c.clock;

        // Collect keys to remove (can't mutate total_bytes inside retain closure).
        let to_remove: Vec<CacheKey> = c
            .entries
            .iter()
            .filter(|(_, entry)| {
                let age = current_clock.saturating_sub(entry.last_use);
                age > max_age && entry.call_count < 10
            })
            .map(|(&k, _)| k)
            .collect();

        let flushed = to_remove.len();
        for key in to_remove {
            if let Some(entry) = c.entries.remove(&key) {
                c.total_bytes = c.total_bytes.saturating_sub(entry.compiled.code_size);
            }
        }
        flushed
    })
}

/// Flush entries with high deopt counts (unstable code).
///
/// Returns the number of entries flushed.
pub fn flush_unstable(max_deopts: u32) -> usize {
    CACHE.with(|cache| {
        let mut c = cache.borrow_mut();

        let to_remove: Vec<CacheKey> = c
            .entries
            .iter()
            .filter(|(_, entry)| entry.deopt_count > max_deopts)
            .map(|(&k, _)| k)
            .collect();

        let flushed = to_remove.len();
        for key in to_remove {
            if let Some(entry) = c.entries.remove(&key) {
                c.total_bytes = c.total_bytes.saturating_sub(entry.compiled.code_size);
            }
        }
        flushed
    })
}
