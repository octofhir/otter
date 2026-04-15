//! Tier-up hook (dynamic-dispatch bridge between interpreter and JIT).
//!
//! The interpreter (`otter-vm`) must not depend on `otter-jit` because of
//! crate layering (`otter-jit` → `otter-vm`). To let the interpreter dispatch
//! inner function calls into cached native code without importing the JIT,
//! `otter-runtime` installs a trait object implementing [`TierUpHook`] into
//! `RuntimeState::tier_up_hook` at runtime startup.
//!
//! Design mirrors JSC's `ExecutableBase`: "execute cached" and "compile" are
//! separate entry points so the hot dispatch path can cheaply check the cache
//! and only request compilation when the hotness budget is exhausted.
//!
//! The trait methods take raw pointers for the register file so the JIT impl
//! can pass them straight into its `extern "C"` native entry points without a
//! second copy. Creating these raw pointers is safe in `otter-vm`; the unsafe
//! dereference happens inside the trait's impl in `otter-jit`.

use std::sync::Arc;

use crate::module::{FunctionIndex, Module};
use crate::value::RegisterValue;

use super::RuntimeState;

/// Result of invoking a cached native function via the tier-up hook.
#[derive(Debug)]
pub enum TierUpExecResult {
    /// Native execution succeeded; caller should treat the value as the
    /// function's return value.
    Return(RegisterValue),
    /// Native code bailed out at `resume_pc` with the given reason code.
    /// The interpreter should resume at this PC with the register file
    /// already materialized in place. `accumulator_raw` is the NaN-boxed
    /// v2 accumulator value captured by the bailout prologue; the
    /// interpreter must load it into the frame's accumulator so v2 dispatch
    /// resumes with the live value.
    Bailout {
        resume_pc: u32,
        reason: u32,
        accumulator_raw: u64,
    },
    /// The function has no cached compiled code; caller must interpret.
    NotCompiled,
}

/// Tier-up bridge installed by the embedding runtime.
///
/// Implementors are expected to be cheap to `Arc::clone` — the interpreter
/// clones the Arc on every inner call to avoid tangling the `RuntimeState`
/// borrow with the hook borrow.
pub trait TierUpHook: Send + Sync {
    /// Invokes a function's cached native code, if any.
    ///
    /// `registers_base` points to the first of `register_count` slots (shared
    /// with the interpreter's active frame). `this_raw` is the NaN-boxed
    /// receiver value. `runtime_ptr` and `interrupt_flag` are forwarded to the
    /// native entry so JIT helpers can reach back into the runtime.
    #[allow(clippy::too_many_arguments)]
    fn execute_cached(
        &self,
        module: &Module,
        function_index: FunctionIndex,
        registers_base: *mut RegisterValue,
        register_count: usize,
        this_raw: u64,
        runtime_ptr: *mut (),
        interrupt_flag: *const u8,
    ) -> TierUpExecResult;

    /// Attempts to compile a function into native code and install it in the
    /// compiled-code cache. Called synchronously from the interpreter's call
    /// path after the hotness budget is exhausted. Returns `true` if there is
    /// now cached code available (either newly compiled or already present).
    fn try_compile(
        &self,
        module: &Module,
        function_index: FunctionIndex,
        runtime_ptr: *mut (),
    ) -> bool;
}

/// JSC-style hotness accounting constants.
///
/// The execution counter receives `TIER1_CALL_COST` per function entry and
/// `TIER1_BACKEDGE_COST` per loop iteration, matching JSC's (`+15` / `+1`)
/// model. Hot threshold ≈ 100 calls OR 1500 back-edges.
///
/// Reference: <https://webkit.org/blog/10308/speculation-in-javascriptcore/>.
pub const TIER1_INITIAL_BUDGET: i32 = 1500;
pub const TIER1_CALL_COST: i32 = 15;
pub const TIER1_BACKEDGE_COST: i32 = 1;

/// Public API methods on `RuntimeState` for the tier-up hot path.
impl RuntimeState {
    /// Installs a tier-up hook. Called once by the embedding runtime after
    /// construction. Overwriting an existing hook is supported.
    pub fn set_tier_up_hook(&mut self, hook: Arc<dyn TierUpHook>) {
        self.tier_up_hook = Some(hook);
    }

    /// Returns a clone of the active tier-up hook, if any. Returning a clone
    /// lets the caller release the `&self` borrow on `RuntimeState` before
    /// invoking mutable-borrowing methods on the runtime.
    pub fn tier_up_hook(&self) -> Option<Arc<dyn TierUpHook>> {
        self.tier_up_hook.clone()
    }

    /// Returns `true` if the function is in the blacklist (will never be
    /// compiled again for this runtime's lifetime).
    pub fn is_tier_up_blacklisted(&self, idx: FunctionIndex) -> bool {
        self.tier_up_blacklisted.contains(&idx)
    }

    /// Adds a function to the tier-up blacklist.
    pub fn blacklist_for_tier_up(&mut self, idx: FunctionIndex) {
        self.tier_up_blacklisted.insert(idx);
        self.tier_up_budgets.remove(&idx);
    }

    /// Decrements the hotness budget for a function by `delta`. Returns
    /// `true` if the budget has reached zero or gone negative (tier-up
    /// candidate).
    ///
    /// Blacklisted functions always return `false` (no further compilation
    /// attempts).
    pub fn decrement_tier_up_budget(&mut self, idx: FunctionIndex, delta: i32) -> bool {
        if self.tier_up_blacklisted.contains(&idx) {
            return false;
        }
        let budget = self
            .tier_up_budgets
            .entry(idx)
            .or_insert(TIER1_INITIAL_BUDGET);
        *budget -= delta;
        *budget <= 0
    }

    /// Resets the budget for a function. Used when a cache miss occurs after
    /// a compile attempt; avoids immediately retrying compilation.
    pub fn reset_tier_up_budget(&mut self, idx: FunctionIndex) {
        if !self.tier_up_blacklisted.contains(&idx) {
            self.tier_up_budgets.insert(idx, TIER1_INITIAL_BUDGET);
        }
    }
}
