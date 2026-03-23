//! JIT configuration: thresholds, tier budgets, and environment variable overrides.

use std::sync::LazyLock;

/// JIT configuration, read once from environment variables at startup.
pub static JIT_CONFIG: LazyLock<JitConfig> = LazyLock::new(JitConfig::from_env);

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

    /// Whether to dump MIR to stderr before codegen.
    /// Override: `OTTER_JIT_DUMP_MIR=1`.
    pub dump_mir: bool,

    /// Whether to dump disassembled machine code to stderr.
    /// Override: `OTTER_JIT_DUMP_ASM=1`.
    pub dump_asm: bool,

    /// Maximum compiled code cache size in bytes (0 = unlimited).
    /// Override: `OTTER_JIT_CODE_CACHE_MB=<N>`.
    pub code_cache_limit_bytes: usize,
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
            dump_mir: false,
            dump_asm: false,
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
        if let Ok(v) = std::env::var("OTTER_JIT_DUMP_MIR") {
            cfg.dump_mir = v == "1";
        }
        if let Ok(v) = std::env::var("OTTER_JIT_DUMP_ASM") {
            cfg.dump_asm = v == "1";
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
