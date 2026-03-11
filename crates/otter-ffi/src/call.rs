//! Low-level FFI call dispatch via libffi.
//!
//! Converts pre-marshaled u64 argument values into libffi `Arg` values,
//! builds a CIF (Call Interface), and invokes the foreign function.

use libffi::middle::{Arg, Cif};

use crate::error::FfiError;
use crate::types::{FFIType, FfiSignature};

/// Invoke a foreign function with pre-marshaled arguments.
///
/// Each element of `arg_values` is a u64 containing the raw bits of the
/// corresponding C argument (sign-extended for smaller integers, bit-cast for
/// floats, raw address for pointers).
///
/// # Safety
///
/// - `fn_ptr` must be a valid function pointer matching `sig`.
/// - `arg_values` must contain correctly marshaled values for each argument type.
/// - The number of `arg_values` must match `sig.args.len()`.
pub unsafe fn ffi_call(
    fn_ptr: *const (),
    sig: &FfiSignature,
    arg_values: &[u64],
) -> Result<u64, FfiError> {
    if arg_values.len() != sig.args.len() {
        return Err(FfiError::ArgCountMismatch {
            expected: sig.args.len(),
            got: arg_values.len(),
        });
    }

    // Build libffi CIF
    let arg_types: Vec<_> = sig.args.iter().map(|t| t.to_libffi_type()).collect();
    let ret_type = sig.returns.to_libffi_type();
    let cif = Cif::new(arg_types, ret_type);

    // Build argument list.
    // libffi's Arg::new takes &T references. We store typed copies in arg_storage
    // and pass references to them. All values live on the stack for the duration
    // of the call.
    let mut arg_storage: Vec<u64> = arg_values.to_vec();
    let args: Vec<Arg> = sig
        .args
        .iter()
        .zip(arg_storage.iter_mut())
        .map(|(_ty, val)| {
            // All C types fit in a u64. libffi reads the correct number of bytes
            // from the pointer based on the CIF arg type, so passing &u64 works
            // for all types on little-endian. On big-endian, we'd need to right-align,
            // but libffi handles this internally when the CIF type is set correctly.
            Arg::new(val)
        })
        .collect();

    // Call the function via libffi
    let code_ptr = libffi::middle::CodePtr::from_ptr(fn_ptr as *const _);

    let result: u64 = match sig.returns {
        FFIType::Void => {
            unsafe { cif.call::<()>(code_ptr, &args) };
            0
        }
        FFIType::Char | FFIType::I8 => {
            let r: i8 = unsafe { cif.call(code_ptr, &args) };
            r as u64
        }
        FFIType::U8 | FFIType::Bool => {
            let r: u8 = unsafe { cif.call(code_ptr, &args) };
            r as u64
        }
        FFIType::I16 => {
            let r: i16 = unsafe { cif.call(code_ptr, &args) };
            r as u64
        }
        FFIType::U16 => {
            let r: u16 = unsafe { cif.call(code_ptr, &args) };
            r as u64
        }
        FFIType::I32 => {
            let r: i32 = unsafe { cif.call(code_ptr, &args) };
            r as u64
        }
        FFIType::U32 => {
            let r: u32 = unsafe { cif.call(code_ptr, &args) };
            r as u64
        }
        FFIType::I64 | FFIType::I64Fast => {
            let r: i64 = unsafe { cif.call(code_ptr, &args) };
            r as u64
        }
        FFIType::U64 | FFIType::U64Fast => unsafe { cif.call(code_ptr, &args) },
        FFIType::F32 => {
            let r: f32 = unsafe { cif.call(code_ptr, &args) };
            r.to_bits() as u64
        }
        FFIType::F64 => {
            let r: f64 = unsafe { cif.call(code_ptr, &args) };
            r.to_bits()
        }
        FFIType::Ptr | FFIType::CString | FFIType::Function => {
            let r: usize = unsafe { cif.call(code_ptr, &args) };
            r as u64
        }
    };

    Ok(result)
}

// ---------------------------------------------------------------------------
// JIT trampoline: pre-built CIF for zero-overhead FFI from JIT code
// ---------------------------------------------------------------------------

/// Pre-built FFI call state for the JIT trampoline.
/// Stores the libffi CIF and type metadata so the trampoline avoids
/// rebuilding them on every call.
pub struct FfiTrampolineData {
    pub cif: Cif,
    pub arg_types: Vec<FFIType>,
    pub return_type: FFIType,
}

impl FfiTrampolineData {
    /// Build a trampoline data from a function signature.
    pub fn new(sig: &FfiSignature) -> Self {
        let libffi_arg_types: Vec<_> = sig.args.iter().map(|t| t.to_libffi_type()).collect();
        let ret_type = sig.returns.to_libffi_type();
        let cif = Cif::new(libffi_arg_types, ret_type);
        Self {
            cif,
            arg_types: sig.args.clone(),
            return_type: sig.returns,
        }
    }
}

// NaN-boxing constants (must match otter-vm-core/src/value.rs)
const NANBOX_TAG_INT32: u64 = 0x7FF8_0001_0000_0000;
const NANBOX_TAG_UNDEFINED: u64 = 0x7FF8_0004_0000_0000;
const NANBOX_TAG_NULL: u64 = 0x7FF8_0002_0000_0000;
const NANBOX_TAG_TRUE: u64 = 0x7FF8_0003_0000_0001;

/// Extract f64 from a NaN-boxed i64 value (for JIT trampoline use).
#[inline]
fn nanbox_to_f64(bits: i64) -> f64 {
    let bits = bits as u64;
    // Check for int32 tag
    if (bits & 0xFFFF_FFFF_0000_0000) == NANBOX_TAG_INT32 {
        return (bits as u32 as i32) as f64;
    }
    // Check for boolean
    if bits == NANBOX_TAG_TRUE {
        return 1.0;
    }
    if bits == NANBOX_TAG_UNDEFINED || bits == NANBOX_TAG_NULL {
        return 0.0;
    }
    // Otherwise it's a raw f64
    f64::from_bits(bits)
}

/// Check if NaN-boxed value is null or undefined.
#[inline]
fn nanbox_is_nullish(bits: i64) -> bool {
    let bits = bits as u64;
    bits == NANBOX_TAG_NULL || bits == NANBOX_TAG_UNDEFINED
}

/// Convert a C raw result to NaN-boxed i64 for the JIT.
#[inline]
fn raw_to_nanbox(raw: u64, return_type: FFIType) -> i64 {
    match return_type {
        FFIType::Void => NANBOX_TAG_UNDEFINED as i64,
        FFIType::Bool => {
            if raw != 0 { NANBOX_TAG_TRUE as i64 } else { 0x7FF8_0003_0000_0000_u64 as i64 }
        }
        FFIType::I8 | FFIType::Char => {
            let n = raw as i8 as f64;
            nanbox_f64(n)
        }
        FFIType::U8 => nanbox_f64(raw as u8 as f64),
        FFIType::I16 => nanbox_f64(raw as i16 as f64),
        FFIType::U16 => nanbox_f64(raw as u16 as f64),
        FFIType::I32 => {
            let n = raw as i32;
            // Use int32 NaN-box for i32 values
            (NANBOX_TAG_INT32 | (n as u32 as u64)) as i64
        }
        FFIType::U32 => nanbox_f64(raw as u32 as f64),
        FFIType::I64 | FFIType::I64Fast => nanbox_f64(raw as i64 as f64),
        FFIType::U64 | FFIType::U64Fast => nanbox_f64(raw as f64),
        FFIType::F32 => nanbox_f64(f32::from_bits(raw as u32) as f64),
        FFIType::F64 => nanbox_f64(f64::from_bits(raw)),
        FFIType::Ptr | FFIType::Function => {
            if raw == 0 {
                NANBOX_TAG_NULL as i64
            } else {
                nanbox_f64(raw as f64)
            }
        }
        FFIType::CString => {
            if raw == 0 {
                NANBOX_TAG_NULL as i64
            } else {
                // CString return from JIT path: bail out (needs string interning which requires GC)
                // Return a sentinel that the JIT helper interprets as "need slow path"
                0x7FFC_0000_0000_0000_u64 as i64 // BAILOUT_SENTINEL
            }
        }
    }
}

/// Convert f64 to NaN-boxed i64 (matches Value::number encoding).
#[inline]
fn nanbox_f64(n: f64) -> i64 {
    if n.is_nan() {
        return 0x7FF8_0000_0000_0000_u64 as i64; // canonical NaN
    }
    // Check if value fits in int32
    if n.fract() == 0.0
        && n >= i32::MIN as f64
        && n <= i32::MAX as f64
        && (n != 0.0 || (1.0_f64 / n).is_sign_positive())
    {
        return (NANBOX_TAG_INT32 | (n as i32 as u32 as u64)) as i64;
    }
    n.to_bits() as i64
}

/// JIT trampoline: called from JIT-compiled code to perform an FFI call.
///
/// # Arguments
/// - `opaque`: pointer to `FfiTrampolineData` (pre-built CIF + type info)
/// - `fn_ptr`: raw C function pointer
/// - `js_args`: pointer to array of NaN-boxed i64 JS argument values
/// - `js_argc`: number of JS arguments
///
/// # Returns
/// NaN-boxed i64 result, or BAILOUT_SENTINEL on error.
///
/// # Safety
/// - `opaque` must point to a valid `FfiTrampolineData`
/// - `fn_ptr` must be a valid C function pointer matching the signature
/// - `js_args` must point to `js_argc` valid NaN-boxed i64 values
#[allow(unsafe_code, unsafe_op_in_unsafe_fn)]
pub unsafe extern "C" fn ffi_jit_trampoline(
    opaque: *const (),
    fn_ptr: usize,
    js_args: *const i64,
    js_argc: u16,
) -> i64 {
    let data = unsafe { &*(opaque as *const FfiTrampolineData) };
    let expected_argc = data.arg_types.len();

    // Marshal JS NaN-boxed values to C raw u64 values
    let mut raw_args: Vec<u64> = Vec::with_capacity(expected_argc);
    for i in 0..expected_argc {
        let js_val = if (i as u16) < js_argc {
            unsafe { *js_args.add(i) }
        } else {
            NANBOX_TAG_UNDEFINED as i64
        };

        let ty = data.arg_types[i];
        let raw = match ty {
            FFIType::I8 | FFIType::Char => nanbox_to_f64(js_val) as i8 as u64,
            FFIType::U8 => nanbox_to_f64(js_val) as u8 as u64,
            FFIType::Bool => (nanbox_to_f64(js_val) != 0.0) as u64,
            FFIType::I16 => nanbox_to_f64(js_val) as i16 as u64,
            FFIType::U16 => nanbox_to_f64(js_val) as u16 as u64,
            FFIType::I32 => nanbox_to_f64(js_val) as i32 as u64,
            FFIType::U32 => nanbox_to_f64(js_val) as u32 as u64,
            FFIType::I64 | FFIType::I64Fast => nanbox_to_f64(js_val) as i64 as u64,
            FFIType::U64 | FFIType::U64Fast => nanbox_to_f64(js_val) as u64,
            FFIType::F32 => (nanbox_to_f64(js_val) as f32).to_bits() as u64,
            FFIType::F64 => nanbox_to_f64(js_val).to_bits(),
            FFIType::Ptr | FFIType::Function => {
                if nanbox_is_nullish(js_val) { 0 } else { nanbox_to_f64(js_val) as u64 }
            }
            FFIType::CString | FFIType::Void => {
                // CString args and void need the slow path (string interning)
                return 0x7FFC_0000_0000_0000_u64 as i64; // BAILOUT_SENTINEL
            }
        };
        raw_args.push(raw);
    }

    // Build libffi args
    let mut arg_storage = raw_args;
    let args: Vec<Arg> = arg_storage.iter_mut().map(|val| Arg::new(val)).collect();

    let code_ptr = libffi::middle::CodePtr::from_ptr(fn_ptr as *const _);

    // Call through pre-built CIF
    let raw_result: u64 = unsafe { match data.return_type {
        FFIType::Void => {
            data.cif.call::<()>(code_ptr, &args);
            0
        }
        FFIType::Char | FFIType::I8 => {
            let r: i8 = data.cif.call(code_ptr, &args);
            r as u64
        }
        FFIType::U8 | FFIType::Bool => {
            let r: u8 = data.cif.call(code_ptr, &args);
            r as u64
        }
        FFIType::I16 => {
            let r: i16 = data.cif.call(code_ptr, &args);
            r as u64
        }
        FFIType::U16 => {
            let r: u16 = data.cif.call(code_ptr, &args);
            r as u64
        }
        FFIType::I32 => {
            let r: i32 = data.cif.call(code_ptr, &args);
            r as u64
        }
        FFIType::U32 => {
            let r: u32 = data.cif.call(code_ptr, &args);
            r as u64
        }
        FFIType::I64 | FFIType::I64Fast => {
            let r: i64 = data.cif.call(code_ptr, &args);
            r as u64
        }
        FFIType::U64 | FFIType::U64Fast => data.cif.call(code_ptr, &args),
        FFIType::F32 => {
            let r: f32 = data.cif.call(code_ptr, &args);
            r.to_bits() as u64
        }
        FFIType::F64 => {
            let r: f64 = data.cif.call(code_ptr, &args);
            r.to_bits()
        }
        FFIType::Ptr | FFIType::CString | FFIType::Function => {
            let r: usize = data.cif.call(code_ptr, &args);
            r as u64
        }
    } };

    raw_to_nanbox(raw_result, data.return_type)
}
