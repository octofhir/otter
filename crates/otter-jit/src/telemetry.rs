//! JIT telemetry — metrics collection for helper calls, deopts, compile times,
//! per-function statistics, and code cache occupancy.
//!
//! All counters are thread-local for zero-contention collection in the
//! single-threaded VM. Call `snapshot()` to read current values.

use std::cell::RefCell;
use std::collections::BTreeMap;

use crate::BailoutReason;

// ============================================================
// Helper family enum
// ============================================================

/// Per-helper-family call counter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum HelperFamily {
    PropertyGet,
    PropertySet,
    ElementGet,
    ElementSet,
    Call,
    Construct,
    Arithmetic,
    Comparison,
    Conversion,
    ObjectAlloc,
    ArrayAlloc,
    ClosureCreate,
    GlobalAccess,
    UpvalueAccess,
    Iterator,
    Exception,
    ClassSuper,
    Other,
}

const HELPER_FAMILY_COUNT: usize = 18;

impl std::fmt::Display for HelperFamily {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

fn helper_family_from_index(i: usize) -> HelperFamily {
    match i {
        0 => HelperFamily::PropertyGet,
        1 => HelperFamily::PropertySet,
        2 => HelperFamily::ElementGet,
        3 => HelperFamily::ElementSet,
        4 => HelperFamily::Call,
        5 => HelperFamily::Construct,
        6 => HelperFamily::Arithmetic,
        7 => HelperFamily::Comparison,
        8 => HelperFamily::Conversion,
        9 => HelperFamily::ObjectAlloc,
        10 => HelperFamily::ArrayAlloc,
        11 => HelperFamily::ClosureCreate,
        12 => HelperFamily::GlobalAccess,
        13 => HelperFamily::UpvalueAccess,
        14 => HelperFamily::Iterator,
        15 => HelperFamily::Exception,
        16 => HelperFamily::ClassSuper,
        _ => HelperFamily::Other,
    }
}

// ============================================================
// Per-function compilation record
// ============================================================

/// Statistics for a single compiled function.
#[derive(Debug, Clone)]
pub struct FunctionStats {
    /// Human-readable function name.
    pub name: String,
    /// Compilation tier (1 = baseline, 2 = optimized).
    pub tier: u8,
    /// Compilation time in nanoseconds.
    pub compile_time_ns: u64,
    /// Generated native code size in bytes.
    pub code_size_bytes: usize,
    /// Number of times this function has been deoptimized.
    pub deopt_count: u64,
    /// Number of recompilations.
    pub recompile_count: u32,
    /// Number of JIT entries (how many times JIT code was entered).
    pub jit_entries: u64,
}

// ============================================================
// Deopt record
// ============================================================

/// A single deopt event (aggregated by site).
#[derive(Debug, Clone)]
pub struct DeoptRecord {
    pub function_name: String,
    pub module_id: u64,
    pub bytecode_pc: u32,
    pub reason: BailoutReason,
    pub count: u64,
}

// ============================================================
// Code cache occupancy
// ============================================================

/// Snapshot of code cache state.
#[derive(Debug, Clone, Default)]
pub struct CodeCacheStats {
    /// Total compiled code bytes across all tiers.
    pub total_bytes: usize,
    /// Number of compiled functions.
    pub function_count: usize,
    /// Tier 1 (baseline) code bytes.
    pub tier1_bytes: usize,
    /// Tier 1 function count.
    pub tier1_count: usize,
    /// Tier 2 (optimized) code bytes.
    pub tier2_bytes: usize,
    /// Tier 2 function count.
    pub tier2_count: usize,
}

// ============================================================
// Aggregated snapshot
// ============================================================

/// Aggregated telemetry snapshot.
#[derive(Debug, Clone, Default)]
pub struct TelemetrySnapshot {
    /// Helper calls by family.
    pub helper_calls: BTreeMap<HelperFamily, u64>,
    /// Deopt count by (function_name, bytecode_pc, reason).
    pub deopts: Vec<DeoptRecord>,
    /// Tier 1 compile time histogram (nanoseconds).
    pub tier1_compile_times_ns: Vec<u64>,
    /// Tier 2 compile time histogram (nanoseconds).
    pub tier2_compile_times_ns: Vec<u64>,
    /// Total entries into JIT code.
    pub jit_entries: u64,
    /// Total entries into interpreter.
    pub interpreter_entries: u64,
    /// Per-function compilation statistics.
    pub functions: Vec<FunctionStats>,
    /// Code cache occupancy.
    pub code_cache: CodeCacheStats,
    /// Deopt histogram: reason -> total count across all sites.
    pub deopt_histogram: BTreeMap<BailoutReason, u64>,
}

// ============================================================
// Thread-local state
// ============================================================

thread_local! {
    static TELEMETRY: RefCell<TelemetryState> = RefCell::new(TelemetryState::default());
}

#[derive(Default)]
struct TelemetryState {
    helper_calls: [u64; HELPER_FAMILY_COUNT],
    deopts: BTreeMap<(u64, u32, u8), DeoptEntry>,
    tier1_compile_times: Vec<u64>,
    tier2_compile_times: Vec<u64>,
    jit_entries: u64,
    interpreter_entries: u64,
    /// Per-function stats keyed by (module_id, function_name).
    functions: BTreeMap<String, FunctionStatsEntry>,
    /// Code cache stats.
    code_cache: CodeCacheStats,
}

struct DeoptEntry {
    function_name: String,
    module_id: u64,
    bytecode_pc: u32,
    reason: BailoutReason,
    count: u64,
}

struct FunctionStatsEntry {
    name: String,
    tier: u8,
    compile_time_ns: u64,
    code_size_bytes: usize,
    deopt_count: u64,
    recompile_count: u32,
    jit_entries: u64,
}

// ============================================================
// Recording API
// ============================================================

/// Record a helper call.
pub fn record_helper_call(family: HelperFamily) {
    TELEMETRY.with(|t| {
        t.borrow_mut().helper_calls[family as usize] += 1;
    });
}

/// Record a deoptimization event.
pub fn record_deopt(function_name: &str, module_id: u64, bytecode_pc: u32, reason: BailoutReason) {
    TELEMETRY.with(|t| {
        let mut state = t.borrow_mut();
        let key = (module_id, bytecode_pc, reason as u8);
        state
            .deopts
            .entry(key)
            .and_modify(|e| e.count += 1)
            .or_insert_with(|| DeoptEntry {
                function_name: function_name.to_string(),
                module_id,
                bytecode_pc,
                reason,
                count: 1,
            });
    });
}

/// Record a compilation time.
pub fn record_compile_time(tier1: bool, duration_ns: u64) {
    TELEMETRY.with(|t| {
        let mut state = t.borrow_mut();
        if tier1 {
            state.tier1_compile_times.push(duration_ns);
        } else {
            state.tier2_compile_times.push(duration_ns);
        }
    });
}

/// Record a JIT entry (function execution started in JIT code).
pub fn record_jit_entry() {
    TELEMETRY.with(|t| {
        t.borrow_mut().jit_entries += 1;
    });
}

/// Record an interpreter entry.
pub fn record_interpreter_entry() {
    TELEMETRY.with(|t| {
        t.borrow_mut().interpreter_entries += 1;
    });
}

/// Record a function compilation (per-function telemetry).
pub fn record_function_compiled(
    name: &str,
    tier: u8,
    compile_time_ns: u64,
    code_size_bytes: usize,
) {
    TELEMETRY.with(|t| {
        let mut state = t.borrow_mut();
        let key = format!("{name}@tier{tier}");
        let entry = state
            .functions
            .entry(key)
            .or_insert_with(|| FunctionStatsEntry {
                name: name.to_string(),
                tier,
                compile_time_ns: 0,
                code_size_bytes: 0,
                deopt_count: 0,
                recompile_count: 0,
                jit_entries: 0,
            });
        entry.compile_time_ns = compile_time_ns;
        entry.code_size_bytes = code_size_bytes;
        entry.recompile_count += 1;
    });
}

/// Record a JIT entry for a specific function.
pub fn record_function_jit_entry(name: &str, tier: u8) {
    TELEMETRY.with(|t| {
        let mut state = t.borrow_mut();
        let key = format!("{name}@tier{tier}");
        if let Some(entry) = state.functions.get_mut(&key) {
            entry.jit_entries += 1;
        }
    });
}

/// Record a deopt for a specific function.
pub fn record_function_deopt(name: &str, tier: u8) {
    TELEMETRY.with(|t| {
        let mut state = t.borrow_mut();
        let key = format!("{name}@tier{tier}");
        if let Some(entry) = state.functions.get_mut(&key) {
            entry.deopt_count += 1;
        }
    });
}

/// Update code cache occupancy stats.
pub fn update_code_cache_stats(stats: CodeCacheStats) {
    TELEMETRY.with(|t| {
        t.borrow_mut().code_cache = stats;
    });
}

// ============================================================
// Snapshot API
// ============================================================

/// Take a snapshot of current telemetry state.
pub fn snapshot() -> TelemetrySnapshot {
    TELEMETRY.with(|t| {
        let state = t.borrow();

        // Helper calls.
        let mut helper_calls = BTreeMap::new();
        for (i, &count) in state.helper_calls.iter().enumerate() {
            if count > 0 {
                helper_calls.insert(helper_family_from_index(i), count);
            }
        }

        // Deopts.
        let deopts: Vec<DeoptRecord> = state
            .deopts
            .values()
            .map(|e| DeoptRecord {
                function_name: e.function_name.clone(),
                module_id: e.module_id,
                bytecode_pc: e.bytecode_pc,
                reason: e.reason,
                count: e.count,
            })
            .collect();

        // Deopt histogram (aggregate by reason).
        let mut deopt_histogram: BTreeMap<BailoutReason, u64> = BTreeMap::new();
        for d in &deopts {
            *deopt_histogram.entry(d.reason).or_default() += d.count;
        }

        // Per-function stats.
        let functions: Vec<FunctionStats> = state
            .functions
            .values()
            .map(|e| FunctionStats {
                name: e.name.clone(),
                tier: e.tier,
                compile_time_ns: e.compile_time_ns,
                code_size_bytes: e.code_size_bytes,
                deopt_count: e.deopt_count,
                recompile_count: e.recompile_count,
                jit_entries: e.jit_entries,
            })
            .collect();

        TelemetrySnapshot {
            helper_calls,
            deopts,
            tier1_compile_times_ns: state.tier1_compile_times.clone(),
            tier2_compile_times_ns: state.tier2_compile_times.clone(),
            jit_entries: state.jit_entries,
            interpreter_entries: state.interpreter_entries,
            functions,
            code_cache: state.code_cache.clone(),
            deopt_histogram,
        }
    })
}

/// Reset all telemetry counters to zero.
pub fn reset() {
    TELEMETRY.with(|t| {
        *t.borrow_mut() = TelemetryState::default();
    });
}

// ============================================================
// Snapshot methods
// ============================================================

impl TelemetrySnapshot {
    /// Ratio of JIT entries to total entries (0.0 to 1.0).
    pub fn native_execution_ratio(&self) -> f64 {
        let total = self.jit_entries + self.interpreter_entries;
        if total == 0 {
            return 0.0;
        }
        self.jit_entries as f64 / total as f64
    }

    /// Total helper calls across all families.
    pub fn total_helper_calls(&self) -> u64 {
        self.helper_calls.values().sum()
    }

    /// Top N helper families by call count.
    pub fn top_helper_families(&self, n: usize) -> Vec<(HelperFamily, u64)> {
        let mut families: Vec<_> = self.helper_calls.iter().map(|(&f, &c)| (f, c)).collect();
        families.sort_by(|a, b| b.1.cmp(&a.1));
        families.truncate(n);
        families
    }

    /// Top N deopt sites by count.
    pub fn top_deopt_sites(&self, n: usize) -> Vec<&DeoptRecord> {
        let mut deopts: Vec<_> = self.deopts.iter().collect();
        deopts.sort_by(|a, b| b.count.cmp(&a.count));
        deopts.truncate(n);
        deopts
    }

    /// Median Tier 1 compile time in nanoseconds, or 0 if none.
    pub fn median_tier1_compile_ns(&self) -> u64 {
        median(&self.tier1_compile_times_ns)
    }

    /// Median Tier 2 compile time in nanoseconds, or 0 if none.
    pub fn median_tier2_compile_ns(&self) -> u64 {
        median(&self.tier2_compile_times_ns)
    }

    /// Print a comprehensive human-readable report to stderr.
    ///
    /// Triggered by `OTTER_JIT_DUMP_STATS=1`.
    pub fn dump(&self) {
        eprintln!("========================================================");
        eprintln!("              JIT Telemetry Report                      ");
        eprintln!("========================================================");
        eprintln!();

        // ---- Execution ratio ----
        eprintln!("── Execution ──");
        eprintln!(
            "  Native ratio:  {:.1}%  ({} JIT / {} interpreter entries)",
            self.native_execution_ratio() * 100.0,
            self.jit_entries,
            self.interpreter_entries,
        );
        eprintln!();

        // ---- Helper calls ----
        let total_helpers = self.total_helper_calls();
        if total_helpers > 0 {
            eprintln!("── Helper Calls ({total_helpers} total) ──");
            for (family, count) in self.top_helper_families(10) {
                let pct = count as f64 / total_helpers as f64 * 100.0;
                eprintln!("  {family:<16} {count:>8}  ({pct:>5.1}%)");
            }
            eprintln!();
        }

        // ---- Deopt histogram (aggregate by reason) ----
        if !self.deopt_histogram.is_empty() {
            let total_deopts: u64 = self.deopt_histogram.values().sum();
            eprintln!("── Deopt Histogram ({total_deopts} total) ──");
            // Sort by count descending.
            let mut reasons: Vec<_> = self.deopt_histogram.iter().collect();
            reasons.sort_by(|a, b| b.1.cmp(a.1));
            for &(reason, &count) in &reasons {
                let pct = count as f64 / total_deopts as f64 * 100.0;
                let reason_str = format!("{reason:?}");
                eprintln!("  {reason_str:<24} {count:>6}  ({pct:>5.1}%)");
            }
            eprintln!();
        }

        // ---- Top deopt sites ----
        if !self.deopts.is_empty() {
            eprintln!("── Top Deopt Sites ──");
            for deopt in self.top_deopt_sites(10) {
                let reason_str = format!("{:?}", deopt.reason);
                eprintln!(
                    "  {}@pc{:<4}  {:<24} x{}",
                    deopt.function_name, deopt.bytecode_pc, reason_str, deopt.count,
                );
            }
            eprintln!();
        }

        // ---- Compile latency ----
        eprintln!("── Compilation ──");
        let t1_count = self.tier1_compile_times_ns.len();
        let t2_count = self.tier2_compile_times_ns.len();
        eprintln!(
            "  Tier 1: {t1_count} compilations, median {:.1}ms",
            self.median_tier1_compile_ns() as f64 / 1_000_000.0,
        );
        if t2_count > 0 {
            eprintln!(
                "  Tier 2: {t2_count} compilations, median {:.1}ms",
                self.median_tier2_compile_ns() as f64 / 1_000_000.0,
            );
        }
        eprintln!();

        // ---- Per-function stats ----
        if !self.functions.is_empty() {
            eprintln!("── Per-Function Stats ──");
            eprintln!(
                "  {:<30} {:>5} {:>10} {:>8} {:>7} {:>8}",
                "Function", "Tier", "CompileMs", "CodeB", "Deopts", "Entries"
            );
            eprintln!("  {}", "-".repeat(72));
            let mut sorted = self.functions.clone();
            sorted.sort_by(|a, b| b.jit_entries.cmp(&a.jit_entries));
            for f in sorted.iter().take(20) {
                let name = if f.name.len() > 28 {
                    format!("{}...", &f.name[..25])
                } else {
                    f.name.clone()
                };
                eprintln!(
                    "  {name:<30} {:>5} {:>9.2} {:>8} {:>7} {:>8}",
                    f.tier,
                    f.compile_time_ns as f64 / 1_000_000.0,
                    f.code_size_bytes,
                    f.deopt_count,
                    f.jit_entries,
                );
            }
            eprintln!();
        }

        // ---- Code cache ----
        let cc = &self.code_cache;
        if cc.function_count > 0 {
            eprintln!("── Code Cache ──");
            eprintln!(
                "  Total: {} functions, {} bytes ({:.1} KB)",
                cc.function_count,
                cc.total_bytes,
                cc.total_bytes as f64 / 1024.0,
            );
            eprintln!(
                "  Tier 1: {} functions, {} bytes",
                cc.tier1_count, cc.tier1_bytes,
            );
            eprintln!(
                "  Tier 2: {} functions, {} bytes",
                cc.tier2_count, cc.tier2_bytes,
            );
            eprintln!();
        }
    }
}

fn median(data: &[u64]) -> u64 {
    if data.is_empty() {
        return 0;
    }
    let mut sorted = data.to_vec();
    sorted.sort_unstable();
    sorted[sorted.len() / 2]
}
