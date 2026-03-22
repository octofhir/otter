//! NaN-boxing value tag constants.
//!
//! These constants define the bit patterns used for NaN-boxed value
//! representation. They are shared between `otter-vm-core` (Value type)
//! and `otter-vm-jit` (baseline compiler code generation).

/// Quiet NaN prefix (upper 13 bits = 0x7FF8)
pub const QUIET_NAN: u64 = 0x7FF8_0000_0000_0000;

/// Tag mask — upper 16 bits
pub const TAG_MASK: u64 = 0xFFFF_0000_0000_0000;

/// Payload mask — lower 48 bits
pub const PAYLOAD_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;

// ---- Primitive tags (singleton sentinel values) ----

/// `undefined`
pub const TAG_UNDEFINED: u64 = 0x7FF8_0000_0000_0000;

/// `null`
pub const TAG_NULL: u64 = 0x7FF8_0000_0000_0001;

/// `true`
pub const TAG_TRUE: u64 = 0x7FF8_0000_0000_0002;

/// `false`
pub const TAG_FALSE: u64 = 0x7FF8_0000_0000_0003;

/// Array hole sentinel (not user-visible)
pub const TAG_HOLE: u64 = 0x7FF8_0000_0000_0004;

/// Canonical NaN (distinct from undefined)
pub const TAG_NAN: u64 = 0x7FFA_0000_0000_0000;

// ---- Int32 NaN-boxing ----

/// Int32 tag — upper 32 bits = `0x7FF8_0001`, lower 32 bits = the i32 value
pub const TAG_INT32: u64 = 0x7FF8_0001_0000_0000;

/// Mask to isolate the Int32 tag (upper 32 bits)
pub const INT32_TAG_MASK: u64 = 0xFFFF_FFFF_0000_0000;

// ---- Pointer sub-tags ----

/// Any pointer (test: `(bits & TAG_PTR_MASK) == TAG_POINTER`)
pub const TAG_POINTER: u64 = 0x7FFC_0000_0000_0000;

/// JsObject (plain or array)
pub const TAG_PTR_OBJECT: u64 = 0x7FFC_0000_0000_0000;

/// JsString
pub const TAG_PTR_STRING: u64 = 0x7FFD_0000_0000_0000;

/// Closure or NativeFunctionObject
pub const TAG_PTR_FUNCTION: u64 = 0x7FFE_0000_0000_0000;

/// Everything else (read GcHeader for subtype)
pub const TAG_PTR_OTHER: u64 = 0x7FFF_0000_0000_0000;

/// Mask to test for any pointer sub-tag
pub const TAG_PTR_MASK: u64 = 0xFFFC_0000_0000_0000;
