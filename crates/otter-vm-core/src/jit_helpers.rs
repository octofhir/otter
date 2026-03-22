//! `extern "C"` helper functions called by JIT-compiled code.
//!
//! Each helper takes a `*mut otter_jit::context::JitContext` as first arg,
//! followed by NaN-boxed i64 operands, and returns a NaN-boxed i64 result.
//!
//! These are cold exits — the JIT code calls them for operations that
//! can't be inlined into native code.

use crate::interpreter::Interpreter;
use crate::context::VmContext;
use crate::value::Value;
use otter_jit::context::JitContext;

/// Extract Interpreter and VmContext from a JitContext.
///
/// # Safety
/// The JitContext must have valid interpreter and vm_ctx pointers.
unsafe fn extract_ctx(ctx: *mut JitContext) -> (&'static Interpreter, &'static mut VmContext) {
    let jit_ctx = unsafe { &*ctx };
    let interp = unsafe { &*(jit_ctx.interpreter as *const Interpreter) };
    let vm_ctx = unsafe { &mut *(jit_ctx.vm_ctx as *mut VmContext) };
    (interp, vm_ctx)
}

/// Convert raw NaN-boxed i64 to Value.
fn val(bits: i64) -> Value {
    unsafe { Value::from_raw_bits_unchecked(bits as u64) }.unwrap_or_else(Value::undefined)
}

/// Convert Value to NaN-boxed i64 for return.
fn ret(v: Value) -> i64 {
    v.to_jit_bits()
}

// ============================================================
// Generic Arithmetic
// ============================================================

#[unsafe(no_mangle)]
pub extern "C" fn otter_jit_generic_add(ctx: *mut JitContext, lhs: i64, rhs: i64) -> i64 {
    let (interp, vm_ctx) = unsafe { extract_ctx(ctx) };
    let l = val(lhs);
    let r = val(rhs);
    match interp.op_add(vm_ctx, &l, &r) {
        Ok(v) => ret(v),
        Err(_) => ret(Value::undefined()),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn otter_jit_generic_sub(ctx: *mut JitContext, lhs: i64, rhs: i64) -> i64 {
    let (interp, vm_ctx) = unsafe { extract_ctx(ctx) };
    let l = val(lhs);
    let r = val(rhs);
    match interp.op_sub(vm_ctx, &l, &r) {
        Ok(v) => ret(v),
        Err(_) => ret(Value::undefined()),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn otter_jit_generic_mul(ctx: *mut JitContext, lhs: i64, rhs: i64) -> i64 {
    let (interp, vm_ctx) = unsafe { extract_ctx(ctx) };
    let l = val(lhs);
    let r = val(rhs);
    match interp.op_mul(vm_ctx, &l, &r) {
        Ok(v) => ret(v),
        Err(_) => ret(Value::undefined()),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn otter_jit_generic_div(ctx: *mut JitContext, lhs: i64, rhs: i64) -> i64 {
    let (interp, vm_ctx) = unsafe { extract_ctx(ctx) };
    let l = val(lhs);
    let r = val(rhs);
    match interp.op_div(vm_ctx, &l, &r) {
        Ok(v) => ret(v),
        Err(_) => ret(Value::undefined()),
    }
}

// ============================================================
// Generic Comparison
// ============================================================

/// Fast numeric comparison: handles int32/f64 fast paths.
fn numeric_lt(l: &Value, r: &Value) -> bool {
    if let (Some(li), Some(ri)) = (l.as_int32(), r.as_int32()) {
        return li < ri;
    }
    let ln = l.as_number().or_else(|| l.as_int32().map(|i| i as f64)).unwrap_or(f64::NAN);
    let rn = r.as_number().or_else(|| r.as_int32().map(|i| i as f64)).unwrap_or(f64::NAN);
    ln < rn
}

#[unsafe(no_mangle)]
pub extern "C" fn otter_jit_generic_lt(_ctx: *mut JitContext, lhs: i64, rhs: i64) -> i64 {
    ret(Value::boolean(numeric_lt(&val(lhs), &val(rhs))))
}

#[unsafe(no_mangle)]
pub extern "C" fn otter_jit_generic_le(_ctx: *mut JitContext, lhs: i64, rhs: i64) -> i64 {
    // a <= b  ≡  !(b < a) for numeric (NaN handled: NaN < x = false, so !(false) = true which is wrong for NaN)
    // Proper: use <=
    let l = val(lhs);
    let r = val(rhs);
    if let (Some(li), Some(ri)) = (l.as_int32(), r.as_int32()) {
        return ret(Value::boolean(li <= ri));
    }
    let ln = l.as_number().or_else(|| l.as_int32().map(|i| i as f64)).unwrap_or(f64::NAN);
    let rn = r.as_number().or_else(|| r.as_int32().map(|i| i as f64)).unwrap_or(f64::NAN);
    ret(Value::boolean(ln <= rn))
}

#[unsafe(no_mangle)]
pub extern "C" fn otter_jit_generic_gt(_ctx: *mut JitContext, lhs: i64, rhs: i64) -> i64 {
    ret(Value::boolean(numeric_lt(&val(rhs), &val(lhs))))
}

#[unsafe(no_mangle)]
pub extern "C" fn otter_jit_generic_ge(_ctx: *mut JitContext, lhs: i64, rhs: i64) -> i64 {
    let l = val(lhs);
    let r = val(rhs);
    if let (Some(li), Some(ri)) = (l.as_int32(), r.as_int32()) {
        return ret(Value::boolean(li >= ri));
    }
    let ln = l.as_number().or_else(|| l.as_int32().map(|i| i as f64)).unwrap_or(f64::NAN);
    let rn = r.as_number().or_else(|| r.as_int32().map(|i| i as f64)).unwrap_or(f64::NAN);
    ret(Value::boolean(ln >= rn))
}

#[unsafe(no_mangle)]
pub extern "C" fn otter_jit_generic_eq(_ctx: *mut JitContext, lhs: i64, rhs: i64) -> i64 {
    let l = val(lhs);
    let r = val(rhs);
    // Fast path: bitwise equality for same-type primitives
    if lhs == rhs {
        return ret(Value::boolean(true));
    }
    if let (Some(li), Some(ri)) = (l.as_int32(), r.as_int32()) {
        return ret(Value::boolean(li == ri));
    }
    // Fallback: abstract equality is complex, return false for now
    // TODO: full abstract equality
    ret(Value::boolean(false))
}

// ============================================================
// Generic Inc / Dec / Neg
// ============================================================

#[unsafe(no_mangle)]
pub extern "C" fn otter_jit_generic_inc(_ctx: *mut JitContext, val_bits: i64) -> i64 {
    let v = val(val_bits);
    if let Some(n) = v.as_int32() {
        ret(Value::number((n as f64) + 1.0))
    } else if let Some(n) = v.as_number() {
        ret(Value::number(n + 1.0))
    } else {
        ret(Value::number(f64::NAN))
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn otter_jit_generic_dec(_ctx: *mut JitContext, val_bits: i64) -> i64 {
    let v = val(val_bits);
    if let Some(n) = v.as_int32() {
        ret(Value::number((n as f64) - 1.0))
    } else if let Some(n) = v.as_number() {
        ret(Value::number(n - 1.0))
    } else {
        ret(Value::number(f64::NAN))
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn otter_jit_generic_neg(ctx: *mut JitContext, val_bits: i64) -> i64 {
    let (interp, vm_ctx) = unsafe { extract_ctx(ctx) };
    let v = val(val_bits);
    let n = interp.to_number(&v);
    ret(Value::number(-n))
}

// ============================================================
// Generic Mod / Pow
// ============================================================

#[unsafe(no_mangle)]
pub extern "C" fn otter_jit_generic_mod(ctx: *mut JitContext, lhs: i64, rhs: i64) -> i64 {
    let (interp, _vm_ctx) = unsafe { extract_ctx(ctx) };
    let l = interp.to_number(&val(lhs));
    let r = interp.to_number(&val(rhs));
    ret(Value::number(l % r))
}

#[unsafe(no_mangle)]
pub extern "C" fn otter_jit_generic_pow(ctx: *mut JitContext, lhs: i64, rhs: i64) -> i64 {
    let (interp, _vm_ctx) = unsafe { extract_ctx(ctx) };
    let l = interp.to_number(&val(lhs));
    let r = interp.to_number(&val(rhs));
    ret(Value::number(l.powf(r)))
}

/// Collect all helper function pointers into a table for JITBuilder registration.
pub fn collect_helper_symbols() -> Vec<(&'static str, *const u8)> {
    vec![
        ("otter_jit_generic_add", otter_jit_generic_add as *const u8),
        ("otter_jit_generic_sub", otter_jit_generic_sub as *const u8),
        ("otter_jit_generic_mul", otter_jit_generic_mul as *const u8),
        ("otter_jit_generic_div", otter_jit_generic_div as *const u8),
        ("otter_jit_generic_lt", otter_jit_generic_lt as *const u8),
        ("otter_jit_generic_le", otter_jit_generic_le as *const u8),
        ("otter_jit_generic_gt", otter_jit_generic_gt as *const u8),
        ("otter_jit_generic_ge", otter_jit_generic_ge as *const u8),
        ("otter_jit_generic_eq", otter_jit_generic_eq as *const u8),
        ("otter_jit_generic_inc", otter_jit_generic_inc as *const u8),
        ("otter_jit_generic_dec", otter_jit_generic_dec as *const u8),
        ("otter_jit_generic_neg", otter_jit_generic_neg as *const u8),
        ("otter_jit_generic_mod", otter_jit_generic_mod as *const u8),
        ("otter_jit_generic_pow", otter_jit_generic_pow as *const u8),
    ]
}
