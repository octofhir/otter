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
//!
//! # See also
//! - [`crate::number`]
//! - [`crate::bigint`]

use otter_bytecode::Op;

use crate::{
    Frame, Interpreter, JsString, NumberValue, Value, VmError, abstract_ops, bigint, conversion,
    number, read_register, write_register,
};

type BigIntBinop = fn(
    &bigint::BigIntValue,
    &bigint::BigIntValue,
) -> Result<bigint::BigIntValue, bigint::ops::OpError>;

impl Interpreter {
    pub(crate) fn run_numeric_regs(
        &self,
        frame: &mut Frame,
        dst: u16,
        lhs: u16,
        rhs: u16,
        op: fn(NumberValue, NumberValue) -> NumberValue,
        bigint_op: BigIntBinop,
    ) -> Result<(), VmError> {
        let (dst, lhs, rhs) = binop_values(frame, dst, lhs, rhs)?;
        run_numeric_values(frame, dst, lhs, rhs, op, bigint_op)
    }

    pub(crate) fn run_add_regs(
        &self,
        frame: &mut Frame,
        dst: u16,
        lhs: u16,
        rhs: u16,
    ) -> Result<(), VmError> {
        let (dst, lhs, rhs) = binop_values(frame, dst, lhs, rhs)?;
        self.run_add_values(frame, dst, lhs, rhs)
    }

    fn run_add_values(
        &self,
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
        let result = if matches!(lhs, Value::String(_)) || matches!(rhs, Value::String(_)) {
            let l_str = conversion::to_js_string_primitive(&lhs, &self.string_heap)?;
            let r_str = conversion::to_js_string_primitive(&rhs, &self.string_heap)?;
            Value::String(JsString::concat(&l_str, &r_str, &self.string_heap)?)
        } else {
            let lk = abstract_ops::to_numeric_kind(&lhs).ok_or(VmError::TypeMismatch)?;
            let rk = abstract_ops::to_numeric_kind(&rhs).ok_or(VmError::TypeMismatch)?;
            match (lk, rk) {
                (abstract_ops::NumericKind::Num(a), abstract_ops::NumericKind::Num(b)) => {
                    Value::Number(number::add(a, b))
                }
                (abstract_ops::NumericKind::Big(a), abstract_ops::NumericKind::Big(b)) => {
                    Value::BigInt(bigint::ops::add(&a, &b))
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
        frame.pc += 1;
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
        run_compare_values(frame, dst, lhs, rhs, op)
    }

    pub(crate) fn run_ushr_regs(
        &self,
        frame: &mut Frame,
        dst: u16,
        lhs: u16,
        rhs: u16,
    ) -> Result<(), VmError> {
        let (dst, lhs, rhs) = binop_values(frame, dst, lhs, rhs)?;
        let lk = abstract_ops::to_numeric_kind(&lhs).ok_or(VmError::TypeMismatch)?;
        let rk = abstract_ops::to_numeric_kind(&rhs).ok_or(VmError::TypeMismatch)?;
        let result = match (lk, rk) {
            (abstract_ops::NumericKind::Num(a), abstract_ops::NumericKind::Num(b)) => {
                Value::Number(number::shr_logical(a, b))
            }
            _ => return Err(VmError::TypeMismatch),
        };
        write_register(frame, dst, result)?;
        frame.pc += 1;
        Ok(())
    }

    pub(crate) fn run_neg_regs(
        &self,
        frame: &mut Frame,
        dst: u16,
        src: u16,
    ) -> Result<(), VmError> {
        let v = read_register(frame, src)?.clone();
        let value = match abstract_ops::to_numeric_kind(&v).ok_or(VmError::TypeMismatch)? {
            abstract_ops::NumericKind::Num(n) => Value::Number(number::neg(n)),
            abstract_ops::NumericKind::Big(b) => Value::BigInt(bigint::ops::neg(&b)),
        };
        write_register(frame, dst, value)?;
        frame.pc += 1;
        Ok(())
    }

    pub(crate) fn run_bitwise_not_regs(
        &self,
        frame: &mut Frame,
        dst: u16,
        src: u16,
    ) -> Result<(), VmError> {
        let v = read_register(frame, src)?.clone();
        let value = match abstract_ops::to_numeric_kind(&v).ok_or(VmError::TypeMismatch)? {
            abstract_ops::NumericKind::Num(n) => Value::Number(number::bitwise_not(n)),
            abstract_ops::NumericKind::Big(b) => Value::BigInt(bigint::ops::bitwise_not(&b)),
        };
        write_register(frame, dst, value)?;
        frame.pc += 1;
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
        let eq = lhs == rhs;
        write_register(frame, dst, Value::Boolean(eq ^ negate))?;
        frame.pc += 1;
        Ok(())
    }

    pub(crate) fn run_loose_equal_regs(
        &self,
        frame: &mut Frame,
        dst: u16,
        lhs: u16,
        rhs: u16,
        negate: bool,
    ) -> Result<(), VmError> {
        let (dst, lhs, rhs) = binop_values(frame, dst, lhs, rhs)?;
        let eq = abstract_ops::is_loosely_equal(&lhs, &rhs);
        write_register(frame, dst, Value::Boolean(eq ^ negate))?;
        frame.pc += 1;
        Ok(())
    }

    pub(crate) fn run_same_value_regs(
        &self,
        frame: &mut Frame,
        dst: u16,
        lhs: u16,
        rhs: u16,
    ) -> Result<(), VmError> {
        let (dst, lhs, rhs) = binop_values(frame, dst, lhs, rhs)?;
        let result = abstract_ops::same_value(&lhs, &rhs);
        write_register(frame, dst, Value::Boolean(result))?;
        frame.pc += 1;
        Ok(())
    }
}

fn binop_values(
    frame: &Frame,
    dst: u16,
    lhs: u16,
    rhs: u16,
) -> Result<(u16, Value, Value), VmError> {
    let l = read_register(frame, lhs)?.clone();
    let r = read_register(frame, rhs)?.clone();
    Ok((dst, l, r))
}

fn run_numeric_values(
    frame: &mut Frame,
    dst: u16,
    lhs: Value,
    rhs: Value,
    op: fn(NumberValue, NumberValue) -> NumberValue,
    bigint_op: BigIntBinop,
) -> Result<(), VmError> {
    let lnum = abstract_ops::to_numeric_kind(&lhs).ok_or(VmError::TypeMismatch)?;
    let rnum = abstract_ops::to_numeric_kind(&rhs).ok_or(VmError::TypeMismatch)?;
    let result = match (lnum, rnum) {
        (abstract_ops::NumericKind::Num(a), abstract_ops::NumericKind::Num(b)) => {
            Value::Number(op(a, b))
        }
        (abstract_ops::NumericKind::Big(a), abstract_ops::NumericKind::Big(b)) => {
            Value::BigInt(bigint_op(&a, &b).map_err(bigint_to_vm_error)?)
        }
        _ => return Err(VmError::TypeMismatch),
    };
    write_register(frame, dst, result)?;
    frame.pc += 1;
    Ok(())
}

fn run_compare_values(
    frame: &mut Frame,
    dst: u16,
    lhs: Value,
    rhs: Value,
    op: Op,
) -> Result<(), VmError> {
    let truthy = match op {
        Op::LessThan => matches!(
            abstract_ops::abstract_relational_comparison(&lhs, &rhs),
            abstract_ops::RelationalOutcome::LessThan
        ),
        Op::GreaterThan => matches!(
            abstract_ops::abstract_relational_comparison(&rhs, &lhs),
            abstract_ops::RelationalOutcome::LessThan
        ),
        Op::LessEq => matches!(
            abstract_ops::abstract_relational_comparison(&rhs, &lhs),
            abstract_ops::RelationalOutcome::NotLessThan
        ),
        Op::GreaterEq => matches!(
            abstract_ops::abstract_relational_comparison(&lhs, &rhs),
            abstract_ops::RelationalOutcome::NotLessThan
        ),
        _ => unreachable!("run_compare_values called with non-relational op"),
    };
    write_register(frame, dst, Value::Boolean(truthy))?;
    frame.pc += 1;
    Ok(())
}

pub(crate) fn bigint_sub_op(
    a: &bigint::BigIntValue,
    b: &bigint::BigIntValue,
) -> Result<bigint::BigIntValue, bigint::ops::OpError> {
    Ok(bigint::ops::sub(a, b))
}

pub(crate) fn bigint_mul_op(
    a: &bigint::BigIntValue,
    b: &bigint::BigIntValue,
) -> Result<bigint::BigIntValue, bigint::ops::OpError> {
    Ok(bigint::ops::mul(a, b))
}

pub(crate) fn bigint_and_op(
    a: &bigint::BigIntValue,
    b: &bigint::BigIntValue,
) -> Result<bigint::BigIntValue, bigint::ops::OpError> {
    Ok(bigint::ops::bitwise_and(a, b))
}

pub(crate) fn bigint_or_op(
    a: &bigint::BigIntValue,
    b: &bigint::BigIntValue,
) -> Result<bigint::BigIntValue, bigint::ops::OpError> {
    Ok(bigint::ops::bitwise_or(a, b))
}

pub(crate) fn bigint_xor_op(
    a: &bigint::BigIntValue,
    b: &bigint::BigIntValue,
) -> Result<bigint::BigIntValue, bigint::ops::OpError> {
    Ok(bigint::ops::bitwise_xor(a, b))
}

fn bigint_to_vm_error(err: bigint::ops::OpError) -> VmError {
    match err {
        bigint::ops::OpError::DivisionByZero
        | bigint::ops::OpError::NegativeExponent
        | bigint::ops::OpError::ShiftOutOfRange => VmError::TypeMismatch,
    }
}
