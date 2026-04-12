//! JIT configuration: thresholds, tier budgets, and debug dump flags.
//!
//! Configuration is read from environment variables at first access and can be
//! overridden programmatically via [`set_jit_config`] (called by the runtime
//! builder when CLI flags are provided).

use std::cell::RefCell;

thread_local! {
    static JIT_CONFIG_CELL: RefCell<JitConfig> = RefCell::new(JitConfig::from_env());
}

/// Read the current thread-local JIT configuration.
pub fn jit_config() -> JitConfig {
    JIT_CONFIG_CELL.with(|c| c.borrow().clone())
}

/// Override the thread-local JIT configuration. Call this during runtime
/// initialization to apply CLI flags on top of env-var defaults.
pub fn set_jit_config(config: JitConfig) {
    JIT_CONFIG_CELL.with(|c| *c.borrow_mut() = config);
}

/// Apply selective overrides on top of the current config. Fields that are
/// `None` keep their current value; `Some(v)` replaces them.
pub fn apply_overrides(overrides: &JitConfigOverrides) {
    JIT_CONFIG_CELL.with(|c| {
        let mut cfg = c.borrow_mut();
        if let Some(v) = overrides.dump_bytecode { cfg.dump_bytecode = v; }
        if let Some(v) = overrides.dump_mir { cfg.dump_mir = v; }
        if let Some(v) = overrides.dump_clif { cfg.dump_clif = v; }
        if let Some(v) = overrides.dump_asm { cfg.dump_asm = v; }
        if let Some(v) = overrides.dump_jit_stats { cfg.dump_jit_stats = v; }
    });
}

/// All tunable JIT parameters.
#[derive(Debug, Clone)]
pub struct JitConfig {
    /// Whether JIT compilation is enabled at all.
    /// Override: `OTTER_JIT=0` to disable.
    pub enabled: bool,

    /// Number of back-edge hits before a function is enqueued for Tier 1 compilation.
    /// Override: `OTTER_JIT_THRESHOLD=<N>`.
    pub tier1_threshold: u32,

    /// Number of additional executions after Tier 1 before considering Tier 2.
    /// Override: `OTTER_JIT_TIER2_THRESHOLD=<N>`.
    pub tier2_threshold: u32,

    /// Maximum number of times a function can be recompiled before giving up.
    pub max_recompilations: u32,

    /// Maximum number of consecutive deopts before a function is permanently
    /// demoted to interpreter-only.
    pub max_deopts_before_blacklist: u32,

    /// Budget (in back-edge ticks) given to Tier 1 code before checking for tier-up.
    pub tier_up_budget: u32,

    /// Whether to restrict compilation to Tier 1 only (no Tier 2).
    /// Override: `OTTER_JIT_TIER1_ONLY=1`.
    pub tier1_only: bool,

    // ---- Debug dump flags ----

    /// Dump compiled bytecodes before JIT compilation.
    /// Override: `OTTER_JIT_DUMP_BYTECODE=1`.
    pub dump_bytecode: bool,

    /// Dump MIR to stderr before codegen.
    /// Override: `OTTER_JIT_DUMP_MIR=1`.
    pub dump_mir: bool,

    /// Dump Cranelift IR (CLIF) to stderr before native compilation.
    /// Override: `OTTER_JIT_DUMP_CLIF=1`.
    pub dump_clif: bool,

    /// Dump native code hex to stderr after compilation.
    /// Override: `OTTER_JIT_DUMP_ASM=1`.
    pub dump_asm: bool,

    /// Dump JIT telemetry (compile times, bailout counts) on runtime exit.
    /// Override: `OTTER_JIT_DUMP_STATS=1`.
    pub dump_jit_stats: bool,

    /// Dump MIR after each optimization pass.
    /// Override: `OTTER_JIT_DUMP_MIR_PASSES=1`.
    pub dump_mir_passes: bool,

    /// Maximum compiled code cache size in bytes (0 = unlimited).
    /// Override: `OTTER_JIT_CODE_CACHE_MB=<N>`.
    pub code_cache_limit_bytes: usize,
}

/// Selective overrides for JIT debug flags. `None` = keep current value.
#[derive(Debug, Clone, Default)]
pub struct JitConfigOverrides {
    pub dump_bytecode: Option<bool>,
    pub dump_mir: Option<bool>,
    pub dump_clif: Option<bool>,
    pub dump_asm: Option<bool>,
    pub dump_jit_stats: Option<bool>,
}

impl Default for JitConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            tier1_threshold: 100,
            tier2_threshold: 1000,
            max_recompilations: 5,
            max_deopts_before_blacklist: 20,
            tier_up_budget: 500,
            tier1_only: false,
            dump_bytecode: false,
            dump_mir: false,
            dump_clif: false,
            dump_asm: false,
            dump_jit_stats: false,
            dump_mir_passes: false,
            code_cache_limit_bytes: 0,
        }
    }
}

impl JitConfig {
    /// Read configuration from environment variables, falling back to defaults.
    pub fn from_env() -> Self {
        let mut cfg = Self::default();

        if let Ok(v) = std::env::var("OTTER_JIT") {
            cfg.enabled = v != "0";
        }
        if let Ok(v) = std::env::var("OTTER_JIT_THRESHOLD")
            && let Ok(n) = v.parse::<u32>()
        {
            cfg.tier1_threshold = n;
        }
        if let Ok(v) = std::env::var("OTTER_JIT_TIER2_THRESHOLD")
            && let Ok(n) = v.parse::<u32>()
        {
            cfg.tier2_threshold = n;
        }
        if let Ok(v) = std::env::var("OTTER_JIT_TIER1_ONLY") {
            cfg.tier1_only = v == "1";
        }
        if let Ok(v) = std::env::var("OTTER_JIT_DUMP_BYTECODE") {
            cfg.dump_bytecode = v == "1";
        }
        if let Ok(v) = std::env::var("OTTER_JIT_DUMP_MIR") {
            cfg.dump_mir = v == "1";
        }
        if let Ok(v) = std::env::var("OTTER_JIT_DUMP_CLIF") {
            cfg.dump_clif = v == "1";
        }
        if let Ok(v) = std::env::var("OTTER_JIT_DUMP_ASM") {
            cfg.dump_asm = v == "1";
        }
        if let Ok(v) = std::env::var("OTTER_JIT_DUMP_STATS") {
            cfg.dump_jit_stats = v == "1";
        }
        if let Ok(v) = std::env::var("OTTER_JIT_DUMP_MIR_PASSES") {
            cfg.dump_mir_passes = v == "1";
        }
        if let Ok(v) = std::env::var("OTTER_JIT_CODE_CACHE_MB")
            && let Ok(n) = v.parse::<usize>()
        {
            cfg.code_cache_limit_bytes = n * 1024 * 1024;
        }
        if let Ok(v) = std::env::var("OTTER_JIT_MAX_RECOMPILE")
            && let Ok(n) = v.parse::<u32>()
        {
            cfg.max_recompilations = n;
        }

        cfg
    }
}
