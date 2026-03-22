//! Entry ABI definition for JIT-compiled functions.
//!
//! All compiled code uses the same calling convention:
//!
//! ```text
//! extern "C" fn(ctx: *mut JitContext) -> u64
//! ```
//!
//! - Input: pointer to a caller-allocated `JitContext`
//! - Output: NaN-boxed return value (u64), or `BAILOUT_SENTINEL` on deopt
//!
//! This ABI is shared by Tier 1 and Tier 2. No tier-specific conventions.

use cranelift_codegen::ir::{AbiParam, Signature, types};
use cranelift_codegen::isa::CallConv;

/// Build the Cranelift signature for a JIT-compiled function.
///
/// `extern "C" fn(ctx: *mut JitContext) -> u64`
pub fn jit_function_signature(call_conv: CallConv, pointer_type: types::Type) -> Signature {
    let mut sig = Signature::new(call_conv);
    // First arg: pointer to JitContext
    sig.params.push(AbiParam::new(pointer_type));
    // Return value: NaN-boxed u64
    sig.returns.push(AbiParam::new(types::I64));
    sig
}

/// Build the Cranelift signature for a runtime helper call.
///
/// Helpers have varying signatures, but all take `*mut JitContext` as first arg
/// and return `i64` (NaN-boxed value or status code).
pub fn helper_signature(
    call_conv: CallConv,
    pointer_type: types::Type,
    extra_args: usize,
) -> Signature {
    let mut sig = Signature::new(call_conv);
    // First arg: JitContext pointer
    sig.params.push(AbiParam::new(pointer_type));
    // Extra args: NaN-boxed i64 values
    for _ in 0..extra_args {
        sig.params.push(AbiParam::new(types::I64));
    }
    // Return: NaN-boxed i64
    sig.returns.push(AbiParam::new(types::I64));
    sig
}
