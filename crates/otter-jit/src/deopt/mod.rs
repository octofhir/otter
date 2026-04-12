//! Deoptimization metadata and bailout contracts.

pub mod bailout;
pub mod materialize;
pub mod resume;

pub use bailout::{BAILOUT_SENTINEL, BailoutReason};
pub use resume::{
    DeoptError, execute_function_profiled_with_fallback, execute_function_with_fallback,
    execute_module_entry_with_runtime, handoff_for_bailout, resume_function,
};
