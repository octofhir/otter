//! Deoptimization metadata placeholders for the new VM.

/// Stable identifier of a deoptimization site in compiled code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DeoptId(pub u32);
