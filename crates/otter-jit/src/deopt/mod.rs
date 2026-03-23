//! Deoptimization metadata and bailout contracts.

pub mod bailout;
pub mod resume;

pub use bailout::{BAILOUT_SENTINEL, BailoutReason};
pub use resume::{
    NextDeoptError, execute_next_function_profiled_with_fallback,
    execute_next_function_with_fallback, handoff_for_next_bailout, resume_next_function,
};
