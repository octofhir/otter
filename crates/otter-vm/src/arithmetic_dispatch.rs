//! Arithmetic and relational opcode helpers.
//!
//! The main dispatch loop decodes executable operands, then delegates the
//! semantic parts of numeric, BigInt, string-concat, and relational operators
//! here. Keeping these helpers out of `lib.rs` makes the dispatch file easier
//! to shrink without changing VM behavior.
//!
//! # Contents
//! - Register-based binary numeric dispatch.
//! - `+` string-or-numeric dispatch.
//! - Relational comparison dispatch.
//! - BigInt adapter functions used by opcode arms.
//!
//! # Invariants
//! - Inputs are already compiler-lowered through the required ToPrimitive
//!   opcodes before reaching these helpers.
//! - Mixed Number/BigInt arithmetic is rejected with `TypeMismatch`.
//! - BigInt ops here operate on owned [`num_bigint::BigInt`] values
//!   (already cloned out of the GC body by
//!   [`abstract_ops::to_numeric_kind`]); the result is folded back
//!   into a fresh [`bigint::BigIntValue`] handle at the call site.
//!
//! # See also
//! - [`crate::number`]
//! - [`crate::bigint`]

use otter_bytecode::Op;

use crate::{
    Frame, Interpreter, JsString, NumberValue, Value, VmError, abstract_ops, bigint, conversion,
    number, oom_to_vm, read_register, write_register,
};

/// Signature of every BigInt binary op routed through this module.
///
/// Operates on borrowed [`num_bigint::BigInt`] payloads (extracted
/// from the GC body by the caller via
/// [`bigint::BigIntValue::clone_inner`] inside
/// [`abstract_ops::to_numeric_kind`]). Returns an owned `BigInt`;
/// the caller wraps it through [`bigint::BigIntValue::from_inner`].
pub(crate) type BigIntBinop = fn(
    &num_bigint::BigInt,
    &num_bigint::BigInt,
) -> Result<num_bigint::BigInt, bigint::ops::OpError>;

impl Interpreter {
    pub(crate) fn run_numeric_regs(
        &mut self,
        frame: &mut Frame,
        dst: u16,
        lhs: u16,
        rhs: u16,
        op: fn(NumberValue, NumberValue) -> NumberValue,
        bigint_op: BigIntBinop,
    ) -> Result<(), VmError> {
        let (dst, lhs, rhs) = binop_values(frame, dst, lhs, rhs)?;
        let byte_len = self.current_byte_len;
        run_numeric_values(self, frame, dst, lhs, rhs, op, bigint_op, byte_len)
    }

    pub(crate) fn run_add_regs(
        &mut self,
        frame: &mut Frame,
        dst: u16,
        lhs: u16,
        rhs: u16,
    ) -> Result<(), VmError> {
        let (dst, lhs, rhs) = binop_values(frame, dst, lhs, rhs)?;
        self.run_add_values(frame, dst, lhs, rhs)
    }

    fn run_add_values(
        &mut self,
        frame: &mut Frame,
        dst: u16,
        lhs: Value,
        rhs: Value,
    ) -> Result<(), VmError> {
        // §13.15.4 ApplyStringOrNumericBinaryOperator for `+`:
        // already-primitive operands enter here (the compiler emits
        // `Op::ToPrimitive(default)` ahead of `Op::Add`). If either
        // primitive is a String, concatenate; otherwise apply ToNumeric
        // to each primitive and fold via the numeric / BigInt rules.
        let result = if lhs.is_string() || rhs.is_string() {
            let l_str = conversion::to_js_string_primitive(&lhs, self.gc_heap_mut())?;
            let r_str = conversion::to_js_string_primitive(&rhs, self.gc_heap_mut())?;
            Value::string(JsString::concat(l_str, r_str, self.gc_heap_mut())?)
        } else {
            let lk =
                abstract_ops::to_numeric_kind(&lhs, &self.gc_heap).ok_or(VmError::TypeMismatch)?;
            let rk =
                abstract_ops::to_numeric_kind(&rhs, &self.gc_heap).ok_or(VmError::TypeMismatch)?;
            match (lk, rk) {
                (abstract_ops::NumericKind::Num(a), abstract_ops::NumericKind::Num(b)) => {
                    Value::number(number::add(a, b))
                }
                (abstract_ops::NumericKind::Big(a), abstract_ops::NumericKind::Big(b)) => {
                    let sum = bigint::ops::add(&a, &b);
                    let handle = bigint::BigIntValue::from_inner(&mut self.gc_heap, sum)
                        .map_err(oom_to_vm)?;
                    Value::big_int(handle)
                }
                // §6.1.6.2 Numeric Type Conversion forbids mixing
                // Number and BigInt operands without an explicit
                // coercion — raise a TypeError per §13.15.4 step 1.b.
                (abstract_ops::NumericKind::Num(_), abstract_ops::NumericKind::Big(_))
                | (abstract_ops::NumericKind::Big(_), abstract_ops::NumericKind::Num(_)) => {
                    return Err(VmError::TypeMismatch);
                }
            }
        };
        write_register(frame, dst, result)?;
        frame.advance_pc(self.current_byte_len)?;
        Ok(())
    }

    pub(crate) fn run_compare_regs(
        &self,
        frame: &mut Frame,
        dst: u16,
        lhs: u16,
        rhs: u16,
        op: Op,
    ) -> Result<(), VmError> {
        let (dst, lhs, rhs) = binop_values(frame, dst, lhs, rhs)?;
        run_compare_values(
            &self.gc_heap,
            frame,
            dst,
            lhs,
            rhs,
            op,
            self.current_byte_len,
        )
    }

    pub(crate) fn run_ushr_regs(
        &self,
        frame: &mut Frame,
        dst: u16,
        lhs: u16,
        rhs: u16,
    ) -> Result<(), VmError> {
        let (dst, lhs, rhs) = binop_values(frame, dst, lhs, rhs)?;
        let lk = abstract_ops::to_numeric_kind(&lhs, &self.gc_heap).ok_or(VmError::TypeMismatch)?;
        let rk = abstract_ops::to_numeric_kind(&rhs, &self.gc_heap).ok_or(VmError::TypeMismatch)?;
        let result = match (lk, rk) {
            (abstract_ops::NumericKind::Num(a), abstract_ops::NumericKind::Num(b)) => {
                Value::number(number::shr_logical(a, b))
            }
            _ => return Err(VmError::TypeMismatch),
        };
        write_register(frame, dst, result)?;
        frame.advance_pc(self.current_byte_len)?;
        Ok(())
    }

    pub(crate) fn run_neg_regs(
        &mut self,
        frame: &mut Frame,
        dst: u16,
        src: u16,
    ) -> Result<(), VmError> {
        let v = *read_register(frame, src)?;
        let value =
            match abstract_ops::to_numeric_kind(&v, &self.gc_heap).ok_or(VmError::TypeMismatch)? {
                abstract_ops::NumericKind::Num(n) => Value::number(number::neg(n)),
                abstract_ops::NumericKind::Big(b) => {
                    let neg = bigint::ops::neg(&b);
                    let handle = bigint::BigIntValue::from_inner(&mut self.gc_heap, neg)
                        .map_err(oom_to_vm)?;
                    Value::big_int(handle)
                }
            };
        write_register(frame, dst, value)?;
        frame.advance_pc(self.current_byte_len)?;
        Ok(())
    }

    pub(crate) fn run_bitwise_not_regs(
        &mut self,
        frame: &mut Frame,
        dst: u16,
        src: u16,
    ) -> Result<(), VmError> {
        let v = *read_register(frame, src)?;
        let value =
            match abstract_ops::to_numeric_kind(&v, &self.gc_heap).ok_or(VmError::TypeMismatch)? {
                abstract_ops::NumericKind::Num(n) => Value::number(number::bitwise_not(n)),
                abstract_ops::NumericKind::Big(b) => {
                    let notted = bigint::ops::bitwise_not(&b);
                    let handle = bigint::BigIntValue::from_inner(&mut self.gc_heap, notted)
                        .map_err(oom_to_vm)?;
                    Value::big_int(handle)
                }
            };
        write_register(frame, dst, value)?;
        frame.advance_pc(self.current_byte_len)?;
        Ok(())
    }

    pub(crate) fn run_equal_regs(
        &self,
        frame: &mut Frame,
        dst: u16,
        lhs: u16,
        rhs: u16,
        negate: bool,
    ) -> Result<(), VmError> {
        let (dst, lhs, rhs) = binop_values(frame, dst, lhs, rhs)?;
        let eq = abstract_ops::is_strictly_equal(&lhs, &rhs, &self.gc_heap);
        write_register(frame, dst, Value::boolean(eq ^ negate))?;
        frame.advance_pc(self.current_byte_len)?;
        Ok(())
    }

    pub(crate) fn run_loose_equal_regs(
        &mut self,
        context: &crate::execution_context::ExecutionContext,
        frame: &mut Frame,
        dst: u16,
        lhs: u16,
        rhs: u16,
        negate: bool,
    ) -> Result<(), VmError> {
        let (dst, lhs, rhs) = binop_values(frame, dst, lhs, rhs)?;
        let eq = self.loose_equal_with_context(context, &lhs, &rhs)?;
        write_register(frame, dst, Value::boolean(eq ^ negate))?;
        frame.advance_pc(self.current_byte_len)?;
        Ok(())
    }

    /// §7.2.13 `IsLooselyEqual(x, y)` with full access to the
    /// interpreter so the object × primitive ToPrimitive arm can
    /// invoke user-defined `@@toPrimitive` / `valueOf` / `toString`
    /// per §7.1.1. Two object operands compare via reference
    /// identity (§7.2.13 step 1 + IsStrictlyEqual for objects).
    pub(crate) fn loose_equal_with_context(
        &mut self,
        context: &crate::execution_context::ExecutionContext,
        x: &Value,
        y: &Value,
    ) -> Result<bool, VmError> {
        let x_html_dda = x.is_html_dda(&self.gc_heap);
        let y_html_dda = y.is_html_dda(&self.gc_heap);
        if x_html_dda && (y.is_undefined() || y.is_null()) {
            return Ok(true);
        }
        if y_html_dda && (x.is_undefined() || x.is_null()) {
            return Ok(true);
        }
        if abstract_ops::is_primitive(x) && abstract_ops::is_primitive(y) {
            return Ok(abstract_ops::is_loosely_equal(x, y, &self.gc_heap));
        }
        // Two non-primitive operands compare via IsStrictlyEqual,
        // which for objects is reference identity.
        if !abstract_ops::is_primitive(x) && !abstract_ops::is_primitive(y) {
            return Ok(abstract_ops::same_value(x, y, &self.gc_heap));
        }
        // §7.2.13 step 11-12 — Object × primitive: ToPrimitive the
        // object operand with the `default` hint, then recurse over
        // the resulting primitive pair.
        let (lhs_p, rhs_p) = if !abstract_ops::is_primitive(x) {
            let coerced =
                self.evaluate_to_primitive(context, x, abstract_ops::ToPrimitiveHint::Default)?;
            (coerced, *y)
        } else {
            let coerced =
                self.evaluate_to_primitive(context, y, abstract_ops::ToPrimitiveHint::Default)?;
            (*x, coerced)
        };
        Ok(abstract_ops::is_loosely_equal(
            &lhs_p,
            &rhs_p,
            &self.gc_heap,
        ))
    }

    pub(crate) fn run_same_value_regs(
        &self,
        frame: &mut Frame,
        dst: u16,
        lhs: u16,
        rhs: u16,
    ) -> Result<(), VmError> {
        let (dst, lhs, rhs) = binop_values(frame, dst, lhs, rhs)?;
        let result = abstract_ops::same_value(&lhs, &rhs, &self.gc_heap);
        write_register(frame, dst, Value::boolean(result))?;
        frame.advance_pc(self.current_byte_len)?;
        Ok(())
    }
}

fn binop_values(
    frame: &Frame,
    dst: u16,
    lhs: u16,
    rhs: u16,
) -> Result<(u16, Value, Value), VmError> {
    let l = *read_register(frame, lhs)?;
    let r = *read_register(frame, rhs)?;
    Ok((dst, l, r))
}

fn run_numeric_values(
    interp: &mut Interpreter,
    frame: &mut Frame,
    dst: u16,
    lhs: Value,
    rhs: Value,
    op: fn(NumberValue, NumberValue) -> NumberValue,
    bigint_op: BigIntBinop,
    byte_len: u32,
) -> Result<(), VmError> {
    let lnum =
        abstract_ops::to_numeric_kind(&lhs, interp.gc_heap()).ok_or(VmError::TypeMismatch)?;
    let rnum =
        abstract_ops::to_numeric_kind(&rhs, interp.gc_heap()).ok_or(VmError::TypeMismatch)?;
    let result = match (lnum, rnum) {
        (abstract_ops::NumericKind::Num(a), abstract_ops::NumericKind::Num(b)) => {
            Value::number(op(a, b))
        }
        (abstract_ops::NumericKind::Big(a), abstract_ops::NumericKind::Big(b)) => {
            let folded = bigint_op(&a, &b).map_err(|err| bigint_to_vm_error(interp, err))?;
            let handle =
                bigint::BigIntValue::from_inner(interp.gc_heap_mut(), folded).map_err(oom_to_vm)?;
            Value::big_int(handle)
        }
        _ => return Err(VmError::TypeMismatch),
    };
    write_register(frame, dst, result)?;
    frame.advance_pc(byte_len)?;
    Ok(())
}

fn run_compare_values(
    heap: &otter_gc::GcHeap,
    frame: &mut Frame,
    dst: u16,
    lhs: Value,
    rhs: Value,
    op: Op,
    byte_len: u32,
) -> Result<(), VmError> {
    // §7.2.14 step 3.b — relational comparison applies ToNumeric
    // after ToPrimitive(number). Symbols cannot be converted to a
    // numeric value, so all four relational operators throw.
    if lhs.is_symbol() || rhs.is_symbol() {
        return Err(VmError::TypeMismatch);
    }
    let truthy = match op {
        Op::LessThan => matches!(
            abstract_ops::abstract_relational_comparison(&lhs, &rhs, heap),
            abstract_ops::RelationalOutcome::LessThan
        ),
        Op::GreaterThan => matches!(
            abstract_ops::abstract_relational_comparison(&rhs, &lhs, heap),
            abstract_ops::RelationalOutcome::LessThan
        ),
        Op::LessEq => matches!(
            abstract_ops::abstract_relational_comparison(&rhs, &lhs, heap),
            abstract_ops::RelationalOutcome::NotLessThan
        ),
        Op::GreaterEq => matches!(
            abstract_ops::abstract_relational_comparison(&lhs, &rhs, heap),
            abstract_ops::RelationalOutcome::NotLessThan
        ),
        _ => unreachable!("run_compare_values called with non-relational op"),
    };
    write_register(frame, dst, Value::boolean(truthy))?;
    frame.advance_pc(byte_len)?;
    Ok(())
}

pub(crate) fn bigint_sub_op(
    a: &num_bigint::BigInt,
    b: &num_bigint::BigInt,
) -> Result<num_bigint::BigInt, bigint::ops::OpError> {
    Ok(bigint::ops::sub(a, b))
}

pub(crate) fn bigint_mul_op(
    a: &num_bigint::BigInt,
    b: &num_bigint::BigInt,
) -> Result<num_bigint::BigInt, bigint::ops::OpError> {
    Ok(bigint::ops::mul(a, b))
}

pub(crate) fn bigint_and_op(
    a: &num_bigint::BigInt,
    b: &num_bigint::BigInt,
) -> Result<num_bigint::BigInt, bigint::ops::OpError> {
    Ok(bigint::ops::bitwise_and(a, b))
}

pub(crate) fn bigint_or_op(
    a: &num_bigint::BigInt,
    b: &num_bigint::BigInt,
) -> Result<num_bigint::BigInt, bigint::ops::OpError> {
    Ok(bigint::ops::bitwise_or(a, b))
}

pub(crate) fn bigint_xor_op(
    a: &num_bigint::BigInt,
    b: &num_bigint::BigInt,
) -> Result<num_bigint::BigInt, bigint::ops::OpError> {
    Ok(bigint::ops::bitwise_xor(a, b))
}

fn bigint_to_vm_error(interp: &Interpreter, err: bigint::ops::OpError) -> VmError {
    // §6.1.6.2.5 / .3 / .9 — BigInt division and remainder by zero,
    // a negative `**` exponent, and an unrepresentable shift all
    // raise RangeError, not TypeError.
    let message = match err {
        bigint::ops::OpError::DivisionByZero => "Division by zero",
        bigint::ops::OpError::NegativeExponent => "Exponent must be non-negative",
        bigint::ops::OpError::ShiftOutOfRange => "Maximum BigInt size exceeded",
    };
    interp.err_range((message.to_string()).into())
}
