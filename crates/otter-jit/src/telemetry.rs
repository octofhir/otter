//! JIT telemetry — metrics collection for helper calls, deopts, and compile times.
//!
//! All counters are thread-local for zero-contention collection in the
//! single-threaded VM. Call `snapshot()` to read current values.

use std::cell::RefCell;
use std::collections::BTreeMap;

use crate::BailoutReason;

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
    /// Total instructions executed in JIT code (estimated from entry count * avg).
    pub jit_entries: u64,
    /// Total instructions executed in interpreter.
    pub interpreter_entries: u64,
}

/// A single deopt event.
#[derive(Debug, Clone)]
pub struct DeoptRecord {
    pub function_name: String,
    pub module_id: u64,
    pub bytecode_pc: u32,
    pub reason: BailoutReason,
    pub count: u64,
}

thread_local! {
    static TELEMETRY: RefCell<TelemetryState> = RefCell::new(TelemetryState::default());
}

#[derive(Default)]
struct TelemetryState {
    helper_calls: [u64; 18],                      // indexed by HelperFamily as u8
    deopts: BTreeMap<(u64, u32, u8), DeoptEntry>, // (module_id, pc, reason) -> entry
    tier1_compile_times: Vec<u64>,
    tier2_compile_times: Vec<u64>,
    jit_entries: u64,
    interpreter_entries: u64,
}

struct DeoptEntry {
    function_name: String,
    module_id: u64,
    bytecode_pc: u32,
    reason: BailoutReason,
    count: u64,
}

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

/// Take a snapshot of current telemetry state.
pub fn snapshot() -> TelemetrySnapshot {
    TELEMETRY.with(|t| {
        let state = t.borrow();
        let mut helper_calls = BTreeMap::new();
        for (i, &count) in state.helper_calls.iter().enumerate() {
            if count > 0 {
                // Safety: i is always 0..18, matching HelperFamily variants.
                let family = match i {
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
                };
                helper_calls.insert(family, count);
            }
        }

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

        TelemetrySnapshot {
            helper_calls,
            deopts,
            tier1_compile_times_ns: state.tier1_compile_times.clone(),
            tier2_compile_times_ns: state.tier2_compile_times.clone(),
            jit_entries: state.jit_entries,
            interpreter_entries: state.interpreter_entries,
        }
    })
}

/// Reset all telemetry counters to zero.
pub fn reset() {
    TELEMETRY.with(|t| {
        *t.borrow_mut() = TelemetryState::default();
    });
}

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

    /// Print a human-readable summary to stderr.
    pub fn dump(&self) {
        eprintln!("=== JIT Telemetry ===");
        eprintln!(
            "Native ratio: {:.1}% ({} JIT / {} interp)",
            self.native_execution_ratio() * 100.0,
            self.jit_entries,
            self.interpreter_entries,
        );
        eprintln!("Total helper calls: {}", self.total_helper_calls(),);
        eprintln!("Top 10 helper families:");
        for (family, count) in self.top_helper_families(10) {
            eprintln!("  {:?}: {}", family, count);
        }
        eprintln!("Top 10 deopt sites:");
        for deopt in self.top_deopt_sites(10) {
            eprintln!(
                "  {}@pc{}: {:?} (x{})",
                deopt.function_name, deopt.bytecode_pc, deopt.reason, deopt.count,
            );
        }
        eprintln!(
            "Compile latency: Tier1 median {}ns, Tier2 median {}ns",
            self.median_tier1_compile_ns(),
            self.median_tier2_compile_ns(),
        );
        eprintln!(
            "Compilations: {} Tier1, {} Tier2",
            self.tier1_compile_times_ns.len(),
            self.tier2_compile_times_ns.len(),
        );
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

impl std::fmt::Display for HelperFamily {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}
