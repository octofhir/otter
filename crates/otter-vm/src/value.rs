//! Shared register value model for the new VM.
//!
//! The layout intentionally matches the primitive NaN-boxing tags used by the
//! existing runtime so the new VM does not drift onto a second value ABI.

use core::fmt;

/// Error produced by arithmetic or comparison operations on register values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ValueError {
    /// The operation expected 32-bit integer inputs.
    ExpectedI32,
    /// Integer division attempted to divide by zero.
    DivisionByZero,
}

impl fmt::Display for ValueError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ExpectedI32 => f.write_str("operation expected 32-bit integer inputs"),
            Self::DivisionByZero => f.write_str("integer division by zero"),
        }
    }
}

impl std::error::Error for ValueError {}

const QUIET_NAN: u64 = 0x7FF8_0000_0000_0000;
const OBJECT_TAG_MASK: u64 = 0xFFFF_0000_0000_0000;
const OBJECT_PAYLOAD_MASK: u64 = 0x0000_FFFF_FFFF_FFFF;
const TAG_UNDEFINED: u64 = 0x7FF8_0000_0000_0000;
const TAG_NULL: u64 = 0x7FF8_0000_0000_0001;
const TAG_TRUE: u64 = 0x7FF8_0000_0000_0002;
const TAG_FALSE: u64 = 0x7FF8_0000_0000_0003;
const TAG_HOLE: u64 = 0x7FF8_0000_0000_0004;
const TAG_NAN: u64 = 0x7FFA_0000_0000_0000;
const TAG_INT32: u64 = 0x7FF8_0001_0000_0000;
const INT32_TAG_MASK: u64 = 0xFFFF_FFFF_0000_0000;
const TAG_PTR_OBJECT: u64 = 0x7FFC_0000_0000_0000;

/// Shared register value cell for the new VM.
///
/// The current implementation covers the primitive subset needed by the early
/// interpreter while preserving the existing NaN-boxed bit layout for:
///
/// - `undefined`
/// - `null`
/// - booleans
/// - canonical `NaN`
/// - `int32`
/// - plain `f64`
///
/// Object values already use the object-tag namespace, but the payload is a
/// VM-local object handle rather than a heap pointer.
#[derive(Clone, Copy)]
#[repr(transparent)]
pub struct RegisterValue(u64);

impl RegisterValue {
    /// Constructs a value from raw non-pointer NaN-boxed bits.
    #[must_use]
    pub const fn from_raw_bits(bits: u64) -> Option<Self> {
        if (bits & OBJECT_TAG_MASK) == TAG_PTR_OBJECT {
            return Some(Self(bits));
        }

        if bits == TAG_UNDEFINED
            || bits == TAG_NULL
            || bits == TAG_TRUE
            || bits == TAG_FALSE
            || bits == TAG_HOLE
            || bits == TAG_NAN
            || (bits & INT32_TAG_MASK) == TAG_INT32
            || (bits & QUIET_NAN) != QUIET_NAN
        {
            Some(Self(bits))
        } else {
            None
        }
    }

    /// Encodes a 32-bit integer.
    #[must_use]
    pub fn from_i32(value: i32) -> Self {
        Self(TAG_INT32 | (value as u32 as u64))
    }

    /// Encodes a boolean.
    #[must_use]
    pub const fn from_bool(value: bool) -> Self {
        Self(if value { TAG_TRUE } else { TAG_FALSE })
    }

    /// Encodes a number.
    #[must_use]
    pub fn from_number(value: f64) -> Self {
        if value.is_nan() {
            return Self(TAG_NAN);
        }

        if value.fract() == 0.0
            && value >= i32::MIN as f64
            && value <= i32::MAX as f64
            && (value != 0.0 || (1.0_f64 / value).is_sign_positive())
        {
            return Self::from_i32(value as i32);
        }

        Self(value.to_bits())
    }

    /// Encodes `undefined`.
    #[must_use]
    pub const fn undefined() -> Self {
        Self(TAG_UNDEFINED)
    }

    /// Encodes `null`.
    #[must_use]
    pub const fn null() -> Self {
        Self(TAG_NULL)
    }

    /// Encodes an internal array hole marker.
    #[must_use]
    pub const fn hole() -> Self {
        Self(TAG_HOLE)
    }

    /// Encodes a VM-local object handle.
    #[must_use]
    pub const fn from_object_handle(handle: u32) -> Self {
        Self(TAG_PTR_OBJECT | handle as u64)
    }

    /// Returns the raw NaN-boxed bits.
    #[must_use]
    pub const fn raw_bits(self) -> u64 {
        self.0
    }

    /// Decodes the value as an `i32`.
    #[must_use]
    pub const fn as_i32(self) -> Option<i32> {
        if (self.0 & INT32_TAG_MASK) == TAG_INT32 {
            Some((self.0 & 0xFFFF_FFFF) as i32)
        } else {
            None
        }
    }

    /// Decodes the value as a `bool`.
    #[must_use]
    pub const fn as_bool(self) -> Option<bool> {
        match self.0 {
            TAG_TRUE => Some(true),
            TAG_FALSE => Some(false),
            _ => None,
        }
    }

    /// Decodes the value as a VM-local object handle.
    #[must_use]
    pub const fn as_object_handle(self) -> Option<u32> {
        if (self.0 & OBJECT_TAG_MASK) == TAG_PTR_OBJECT {
            Some((self.0 & OBJECT_PAYLOAD_MASK) as u32)
        } else {
            None
        }
    }

    /// Returns whether the value is the internal hole marker.
    #[must_use]
    pub const fn is_hole(self) -> bool {
        self.0 == TAG_HOLE
    }

    /// Decodes the value as a number.
    #[must_use]
    pub fn as_number(self) -> Option<f64> {
        if let Some(value) = self.as_i32() {
            return Some(value as f64);
        }
        if self.0 == TAG_NAN {
            return Some(f64::NAN);
        }
        if !self.is_nan_boxed() {
            return Some(f64::from_bits(self.0));
        }

        None
    }

    /// Returns whether the value is truthy in the minimal VM subset.
    #[must_use]
    pub fn is_truthy(self) -> bool {
        match self.0 {
            TAG_UNDEFINED | TAG_NULL | TAG_FALSE | TAG_NAN | TAG_HOLE => false,
            TAG_TRUE => true,
            _ if (self.0 & INT32_TAG_MASK) == TAG_INT32 => self.as_i32().unwrap_or(0) != 0,
            _ if !self.is_nan_boxed() => {
                let number = f64::from_bits(self.0);
                !number.is_nan() && number != 0.0
            }
            _ => true,
        }
    }

    /// Adds two `i32` values with wrapping semantics.
    pub fn add_i32(self, rhs: Self) -> Result<Self, ValueError> {
        let lhs = self.as_i32().ok_or(ValueError::ExpectedI32)?;
        let rhs = rhs.as_i32().ok_or(ValueError::ExpectedI32)?;
        Ok(Self::from_i32(lhs.wrapping_add(rhs)))
    }

    /// Subtracts two `i32` values with wrapping semantics.
    pub fn sub_i32(self, rhs: Self) -> Result<Self, ValueError> {
        let lhs = self.as_i32().ok_or(ValueError::ExpectedI32)?;
        let rhs = rhs.as_i32().ok_or(ValueError::ExpectedI32)?;
        Ok(Self::from_i32(lhs.wrapping_sub(rhs)))
    }

    /// Multiplies two `i32` values with wrapping semantics.
    pub fn mul_i32(self, rhs: Self) -> Result<Self, ValueError> {
        let lhs = self.as_i32().ok_or(ValueError::ExpectedI32)?;
        let rhs = rhs.as_i32().ok_or(ValueError::ExpectedI32)?;
        Ok(Self::from_i32(lhs.wrapping_mul(rhs)))
    }

    /// Divides two `i32` values.
    pub fn div_i32(self, rhs: Self) -> Result<Self, ValueError> {
        let lhs = self.as_i32().ok_or(ValueError::ExpectedI32)?;
        let rhs = rhs.as_i32().ok_or(ValueError::ExpectedI32)?;

        if rhs == 0 {
            return Err(ValueError::DivisionByZero);
        }

        Ok(Self::from_i32(lhs.wrapping_div(rhs)))
    }

    /// Compares two values for equality.
    #[must_use]
    pub fn eq(self, rhs: Self) -> Self {
        Self::from_bool(self == rhs)
    }

    /// Compares two numeric values with less-than semantics.
    #[must_use]
    pub fn lt(self, rhs: Self) -> Self {
        let lhs = self.as_number().unwrap_or(f64::NAN);
        let rhs = rhs.as_number().unwrap_or(f64::NAN);
        Self::from_bool(lhs < rhs)
    }

    /// Compares two numeric values with greater-than semantics.
    #[must_use]
    pub fn gt(self, rhs: Self) -> Self {
        let lhs = self.as_number().unwrap_or(f64::NAN);
        let rhs = rhs.as_number().unwrap_or(f64::NAN);
        Self::from_bool(lhs > rhs)
    }

    /// Compares two numeric values with greater-than-or-equal semantics.
    #[must_use]
    pub fn gte(self, rhs: Self) -> Self {
        let lhs = self.as_number().unwrap_or(f64::NAN);
        let rhs = rhs.as_number().unwrap_or(f64::NAN);
        Self::from_bool(lhs >= rhs)
    }

    /// Compares two numeric values with less-than-or-equal semantics.
    #[must_use]
    pub fn lte(self, rhs: Self) -> Self {
        let lhs = self.as_number().unwrap_or(f64::NAN);
        let rhs = rhs.as_number().unwrap_or(f64::NAN);
        Self::from_bool(lhs <= rhs)
    }

    /// Computes the remainder of two numeric values.
    #[must_use]
    pub fn js_rem(self, rhs: Self) -> Self {
        let lhs = self.as_number().unwrap_or(f64::NAN);
        let rhs = rhs.as_number().unwrap_or(f64::NAN);
        Self::from_number(lhs % rhs)
    }

    #[must_use]
    const fn is_nan_boxed(self) -> bool {
        (self.0 & QUIET_NAN) == QUIET_NAN
    }
}

impl Default for RegisterValue {
    fn default() -> Self {
        Self::undefined()
    }
}

impl PartialEq for RegisterValue {
    fn eq(&self, other: &Self) -> bool {
        if self.0 == TAG_NAN || other.0 == TAG_NAN {
            return false;
        }

        if self.0 == other.0 {
            return true;
        }

        let self_is_int32 = (self.0 & INT32_TAG_MASK) == TAG_INT32;
        let other_is_int32 = (other.0 & INT32_TAG_MASK) == TAG_INT32;
        if self_is_int32 && other_is_int32 {
            return false;
        }

        let self_is_number = self_is_int32 || (self.0 & QUIET_NAN) != QUIET_NAN;
        let other_is_number = other_is_int32 || (other.0 & QUIET_NAN) != QUIET_NAN;
        if self_is_number && other_is_number {
            let lhs = if self_is_int32 {
                (self.0 as u32 as i32) as f64
            } else {
                f64::from_bits(self.0)
            };
            let rhs = if other_is_int32 {
                (other.0 as u32 as i32) as f64
            } else {
                f64::from_bits(other.0)
            };
            return lhs == rhs;
        }

        false
    }
}

impl fmt::Debug for RegisterValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            TAG_UNDEFINED => f.write_str("undefined"),
            TAG_NULL => f.write_str("null"),
            TAG_TRUE => f.write_str("true"),
            TAG_FALSE => f.write_str("false"),
            TAG_HOLE => f.write_str("<hole>"),
            _ if (self.0 & OBJECT_TAG_MASK) == TAG_PTR_OBJECT => {
                write!(
                    f,
                    "[object#{}]",
                    self.as_object_handle().unwrap_or_default()
                )
            }
            _ if (self.0 & INT32_TAG_MASK) == TAG_INT32 => {
                write!(f, "{}", self.as_i32().unwrap_or_default())
            }
            _ if !self.is_nan_boxed() => write!(f, "{}", f64::from_bits(self.0)),
            TAG_NAN => f.write_str("NaN"),
            _ => write!(f, "<boxed:{:#x}>", self.0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        INT32_TAG_MASK, RegisterValue, TAG_FALSE, TAG_INT32, TAG_NAN, TAG_NULL, TAG_PTR_OBJECT,
        TAG_TRUE, TAG_UNDEFINED, ValueError,
    };

    #[test]
    fn integer_values_round_trip() {
        let value = RegisterValue::from_i32(-17);

        assert_eq!(value.as_i32(), Some(-17));
        assert_eq!(value.as_bool(), None);
        assert_eq!(value.as_number(), Some(-17.0));
    }

    #[test]
    fn boolean_truthiness_and_equality_work() {
        let false_value = RegisterValue::from_bool(false);
        let true_value = RegisterValue::from_bool(true);

        assert!(!false_value.is_truthy());
        assert!(true_value.is_truthy());
        assert_eq!(false_value.eq(true_value).as_bool(), Some(false));
        assert_eq!(
            true_value.eq(RegisterValue::from_bool(true)).as_bool(),
            Some(true)
        );
    }

    #[test]
    fn integer_arithmetic_rejects_non_integer_operands() {
        let result = RegisterValue::from_bool(true).add_i32(RegisterValue::from_i32(1));

        assert_eq!(result, Err(ValueError::ExpectedI32));
    }

    #[test]
    fn wrapper_preserves_shared_nan_boxed_value_model() {
        let value = RegisterValue::from_number(3.5);

        assert_eq!(value.as_number(), Some(3.5));
        assert_eq!(value.raw_bits(), 3.5f64.to_bits());
    }

    #[test]
    fn primitive_tag_bits_match_existing_nan_box_layout() {
        assert_eq!(RegisterValue::undefined().raw_bits(), TAG_UNDEFINED);
        assert_eq!(RegisterValue::null().raw_bits(), TAG_NULL);
        assert_eq!(RegisterValue::from_bool(true).raw_bits(), TAG_TRUE);
        assert_eq!(RegisterValue::from_bool(false).raw_bits(), TAG_FALSE);
        assert_eq!(RegisterValue::from_number(f64::NAN).raw_bits(), TAG_NAN);
        assert_eq!(
            RegisterValue::from_i32(7).raw_bits() & INT32_TAG_MASK,
            TAG_INT32
        );
    }

    #[test]
    fn from_raw_bits_accepts_object_handles_and_rejects_unknown_boxed_values() {
        let object_bits = 0x7FFC_0000_0000_1234_u64;
        let unknown_boxed_bits = 0x7FF9_0000_0000_0000_u64;

        let object =
            RegisterValue::from_raw_bits(object_bits).expect("object handle should decode");

        assert_eq!(object.raw_bits(), object_bits);
        assert_eq!(object.as_object_handle(), Some(0x1234));
        assert_eq!(
            RegisterValue::from_raw_bits(TAG_TRUE),
            Some(RegisterValue::from_bool(true))
        );
        assert_eq!(RegisterValue::from_raw_bits(unknown_boxed_bits), None);
    }

    #[test]
    fn object_handles_round_trip() {
        let value = RegisterValue::from_object_handle(17);

        assert_eq!(value.raw_bits(), TAG_PTR_OBJECT | 17);
        assert_eq!(value.as_object_handle(), Some(17));
        assert_eq!(format!("{value:?}"), "[object#17]");
    }
}
