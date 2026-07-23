//! Arithmetic and relational opcode helpers.
//!
//! The main dispatch loop decodes executable operands, then delegates the
//! semantic parts of numeric, BigInt, string-concat, and relational operators
//! here. Keeping these helpers out of `lib.rs` makes the dispatch file easier
//! to shrink without changing VM behavior.
//!
//! # Contents
//! - [`NumericRuntimeOp`] — fully decoded numeric slow-path requests shared by
//!   native tiers.
//! - Value-in/value-out arithmetic kernels independent of frame storage.
//! - Register-based binary numeric dispatch.
//! - Unary/update feedback publication for optimizing-tier specialization.
//! - `+` string-or-numeric dispatch.
//! - Relational comparison dispatch.
//! - BigInt adapter functions used by opcode arms.
//!
//! # Invariants
//! - Inputs are already compiler-lowered through the required ToPrimitive
//!   opcodes before reaching these helpers.
//! - Raw opcode integers are decoded into [`NumericRuntimeOp`] at the native
//!   ABI boundary; semantic kernels never inspect an untyped opcode.
//! - Generic `+` roots both operands and allocated string intermediates in a
//!   handle scope before any moving-GC allocation.
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
    Frame, Interpreter, JsString, NumberValue, Value, VmError, abstract_ops,
    activation_stack::ActivationStack, bigint, feedback::InstructionFeedbackRecorder, number,
    oom_to_vm, read_register, write_register,
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

/// Fully decoded numeric-family operation requested by native code.
///
/// Binary variants carry a checked register identity and update-expression
/// variants carry the signed delta. This prevents raw opcode or overloaded
/// integer operands from crossing the native ABI into VM semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NumericRuntimeOp {
    /// Numeric or BigInt subtraction.
    Sub {
        /// Right-operand register.
        rhs: u16,
    },
    /// Numeric or BigInt multiplication.
    Mul {
        /// Right-operand register.
        rhs: u16,
    },
    /// Numeric or BigInt division.
    Div {
        /// Right-operand register.
        rhs: u16,
    },
    /// Numeric or BigInt remainder.
    Rem {
        /// Right-operand register.
        rhs: u16,
    },
    /// Numeric or BigInt exponentiation.
    Pow {
        /// Right-operand register.
        rhs: u16,
    },
    /// Numeric or BigInt bitwise AND.
    BitwiseAnd {
        /// Right-operand register.
        rhs: u16,
    },
    /// Numeric or BigInt bitwise OR.
    BitwiseOr {
        /// Right-operand register.
        rhs: u16,
    },
    /// Numeric or BigInt bitwise XOR.
    BitwiseXor {
        /// Right-operand register.
        rhs: u16,
    },
    /// Arithmetic left shift.
    Shl {
        /// Shift-count register.
        rhs: u16,
    },
    /// Arithmetic right shift.
    Shr {
        /// Shift-count register.
        rhs: u16,
    },
    /// Logical unsigned right shift.
    Ushr {
        /// Shift-count register.
        rhs: u16,
    },
    /// Abstract less-than comparison.
    LessThan {
        /// Right-operand register.
        rhs: u16,
    },
    /// Abstract less-than-or-equal comparison.
    LessEq {
        /// Right-operand register.
        rhs: u16,
    },
    /// Abstract greater-than comparison.
    GreaterThan {
        /// Right-operand register.
        rhs: u16,
    },
    /// Abstract greater-than-or-equal comparison.
    GreaterEq {
        /// Right-operand register.
        rhs: u16,
    },
    /// Unary numeric negation.
    Neg,
    /// Unary numeric bitwise complement.
    BitwiseNot,
    /// Update-expression numeric step.
    Increment {
        /// Signed step applied after numeric coercion.
        delta: i32,
    },
}

impl NumericRuntimeOp {
    /// Decode the two overloaded machine words used by the numeric runtime
    /// descriptor. This is intentionally the only raw-opcode decoder.
    pub fn decode_abi(opcode: u64, rhs_or_delta: u64) -> Result<Self, VmError> {
        let opcode = u8::try_from(opcode).map_err(|_| VmError::InvalidOperand)?;
        let rhs = || u16::try_from(rhs_or_delta).map_err(|_| VmError::InvalidOperand);
        match opcode {
            x if x == Op::Sub as u8 => Ok(Self::Sub { rhs: rhs()? }),
            x if x == Op::Mul as u8 => Ok(Self::Mul { rhs: rhs()? }),
            x if x == Op::Div as u8 => Ok(Self::Div { rhs: rhs()? }),
            x if x == Op::Rem as u8 => Ok(Self::Rem { rhs: rhs()? }),
            x if x == Op::Pow as u8 => Ok(Self::Pow { rhs: rhs()? }),
            x if x == Op::BitwiseAnd as u8 => Ok(Self::BitwiseAnd { rhs: rhs()? }),
            x if x == Op::BitwiseOr as u8 => Ok(Self::BitwiseOr { rhs: rhs()? }),
            x if x == Op::BitwiseXor as u8 => Ok(Self::BitwiseXor { rhs: rhs()? }),
            x if x == Op::Shl as u8 => Ok(Self::Shl { rhs: rhs()? }),
            x if x == Op::Shr as u8 => Ok(Self::Shr { rhs: rhs()? }),
            x if x == Op::Ushr as u8 => Ok(Self::Ushr { rhs: rhs()? }),
            x if x == Op::LessThan as u8 => Ok(Self::LessThan { rhs: rhs()? }),
            x if x == Op::LessEq as u8 => Ok(Self::LessEq { rhs: rhs()? }),
            x if x == Op::GreaterThan as u8 => Ok(Self::GreaterThan { rhs: rhs()? }),
            x if x == Op::GreaterEq as u8 => Ok(Self::GreaterEq { rhs: rhs()? }),
            x if x == Op::Neg as u8 => Ok(Self::Neg),
            x if x == Op::BitwiseNot as u8 => Ok(Self::BitwiseNot),
            x if x == Op::Increment as u8 => Ok(Self::Increment {
                delta: rhs_or_delta as u32 as i32,
            }),
            _ => Err(VmError::InvalidOperand),
        }
    }

    /// Register read as the right operand, absent for unary/update operations.
    #[must_use]
    pub const fn rhs_register(self) -> Option<u16> {
        match self {
            Self::Sub { rhs }
            | Self::Mul { rhs }
            | Self::Div { rhs }
            | Self::Rem { rhs }
            | Self::Pow { rhs }
            | Self::BitwiseAnd { rhs }
            | Self::BitwiseOr { rhs }
            | Self::BitwiseXor { rhs }
            | Self::Shl { rhs }
            | Self::Shr { rhs }
            | Self::Ushr { rhs }
            | Self::LessThan { rhs }
            | Self::LessEq { rhs }
            | Self::GreaterThan { rhs }
            | Self::GreaterEq { rhs } => Some(rhs),
            Self::Neg | Self::BitwiseNot | Self::Increment { .. } => None,
        }
    }
}

impl Interpreter {
    /// Convert both operands of a binary operator through `ToPrimitive`, left
    /// operand first.
    ///
    /// The conversions can run arbitrary user code, so both operands are held
    /// in the handle arena across them: a `valueOf` on the left operand may
    /// trigger a moving collection that relocates the right one.
    pub(crate) fn coerce_operand_pair(
        &mut self,
        stack: &mut ActivationStack,
        context: &crate::execution_context::ExecutionContext,
        lhs: Value,
        rhs: Value,
        hint: abstract_ops::ToPrimitiveHint,
    ) -> Result<(Value, Value), VmError> {
        if lhs.is_primitive() && rhs.is_primitive() {
            return Ok((lhs, rhs));
        }
        self.with_handle_scope(|interp, scope| {
            let lhs_handle = interp.scoped_value(scope, lhs);
            let rhs_handle = interp.scoped_value(scope, rhs);
            let left = interp.escape_scoped(lhs_handle);
            let left = interp.evaluate_to_primitive(stack, context, &left, hint)?;
            let left_handle = interp.scoped_value(scope, left);
            let right = interp.escape_scoped(rhs_handle);
            let right = interp.evaluate_to_primitive(stack, context, &right, hint)?;
            let right_handle = interp.scoped_value(scope, right);
            Ok((
                interp.escape_scoped(left_handle),
                interp.escape_scoped(right_handle),
            ))
        })
    }

    /// §13.15.3 steps 3-4 — `ToNumeric` over both operands of a non-additive
    /// numeric, bitwise, or shift operator, left operand first.
    ///
    /// The whole of `ToNumeric(lval)` — its `ToPrimitive(number)` ladder, its
    /// Symbol rejection, and its numeric conversion — completes before the
    /// right operand is touched at all. Splitting it into "both `ToPrimitive`,
    /// then both `ToNumber`" is observably wrong: a left operand whose
    /// `valueOf` yields a Symbol must throw before the right operand's
    /// `valueOf` ever runs.
    pub(crate) fn coerce_numeric_operands(
        &mut self,
        stack: &mut ActivationStack,
        context: &crate::execution_context::ExecutionContext,
        lhs: Value,
        rhs: Value,
    ) -> Result<(Value, Value), VmError> {
        // A Number or BigInt is its own `ToNumeric` result and runs no user
        // code, so the common shape never opens a scope.
        if (lhs.is_number() || lhs.is_big_int()) && (rhs.is_number() || rhs.is_big_int()) {
            return Ok((lhs, rhs));
        }
        self.with_handle_scope(|interp, scope| {
            let lhs_handle = interp.scoped_value(scope, lhs);
            let rhs_handle = interp.scoped_value(scope, rhs);
            let left = interp.escape_scoped(lhs_handle);
            let left = interp.coerce_unary_value(
                stack,
                context,
                left,
                crate::jit_runtime_ops::UnaryCoercionOp::ToNumeric,
            )?;
            let left_handle = interp.scoped_value(scope, left);
            let right = interp.escape_scoped(rhs_handle);
            let right = interp.coerce_unary_value(
                stack,
                context,
                right,
                crate::jit_runtime_ops::UnaryCoercionOp::ToNumeric,
            )?;
            let right_handle = interp.scoped_value(scope, right);
            Ok((
                interp.escape_scoped(left_handle),
                interp.escape_scoped(right_handle),
            ))
        })
    }

    pub(crate) fn run_numeric_regs(
        &mut self,
        stack: &mut ActivationStack,
        context: &crate::execution_context::ExecutionContext,
        top_idx: usize,
        dst: u16,
        lhs: u16,
        rhs: u16,
        op: fn(NumberValue, NumberValue) -> NumberValue,
        bigint_op: BigIntBinop,
        feedback: Option<InstructionFeedbackRecorder<'_>>,
    ) -> Result<(), VmError> {
        let (dst, lhs, rhs) = binop_values(&stack[top_idx], dst, lhs, rhs)?;
        if let Some(feedback) = feedback {
            feedback.record_arith(lhs, rhs);
        }
        // Number x Number is the dominant shape, needs no conversion, and
        // cannot re-enter JavaScript, so it never releases the frame borrow.
        if let (Some(a), Some(b)) = (lhs.as_number(), rhs.as_number()) {
            return commit_frame_result(&mut stack[top_idx], dst, Value::number(op(a, b)));
        }
        let result = numeric_binary_value(self, stack, context, lhs, rhs, op, bigint_op)?;
        commit_frame_result(&mut stack[top_idx], dst, result)
    }

    pub(crate) fn run_add_regs(
        &mut self,
        stack: &mut ActivationStack,
        context: &crate::execution_context::ExecutionContext,
        top_idx: usize,
        dst: u16,
        lhs: u16,
        rhs: u16,
        feedback: Option<InstructionFeedbackRecorder<'_>>,
    ) -> Result<(), VmError> {
        let (dst, lhs, rhs) = binop_values(&stack[top_idx], dst, lhs, rhs)?;
        if let Some(feedback) = feedback {
            feedback.record_arith(lhs, rhs);
        }
        if let (Some(a), Some(b)) = (lhs.as_number(), rhs.as_number()) {
            return commit_frame_result(&mut stack[top_idx], dst, Value::number(number::add(a, b)));
        }
        let result = self.add_value(stack, context, lhs, rhs)?;
        commit_frame_result(&mut stack[top_idx], dst, result)
    }

    /// Evaluate `+` without coupling its observable semantics to frame storage.
    /// §13.15.3 ApplyStringOrNumericBinaryOperator for `+`.
    ///
    /// Step 1 sends both operands through `ToPrimitive(default)`, left first,
    /// before the String-concatenation test decides between concatenation and
    /// numeric addition.
    pub(crate) fn add_value(
        &mut self,
        stack: &mut ActivationStack,
        context: &crate::execution_context::ExecutionContext,
        lhs: Value,
        rhs: Value,
    ) -> Result<Value, VmError> {
        // Number + Number is the dominant shape and allocates nothing, so it
        // needs neither the handle scope nor the `ToNumeric` ladder: both
        // operands are already the `Num`/`Num` case below.
        if let (Some(a), Some(b)) = (lhs.as_number(), rhs.as_number()) {
            return Ok(Value::number(number::add(a, b)));
        }
        let (lhs, rhs) = self.coerce_operand_pair(
            stack,
            context,
            lhs,
            rhs,
            abstract_ops::ToPrimitiveHint::Default,
        )?;
        self.with_handle_scope(|interp, scope| {
            let lhs = interp.scoped_value(scope, lhs);
            let rhs = interp.scoped_value(scope, rhs);
            let lhs_value = interp.escape_scoped(lhs);
            let rhs_value = interp.escape_scoped(rhs);

            // §13.15.4 ApplyStringOrNumericBinaryOperator for `+`:
            // already-primitive operands enter here (the compiler emits
            // `Op::ToPrimitive(default)` ahead of `Op::Add`). If either
            // primitive is a String, concatenate; otherwise apply ToNumeric
            // to each primitive and fold via the numeric / BigInt rules.
            if lhs_value.is_string() || rhs_value.is_string() {
                if let Some(fast) = interp.try_concat_string_int32(lhs_value, rhs_value) {
                    return fast.map_err(oom_to_vm);
                }

                let lhs_string = interp.js_string_for_concat(lhs_value)?;
                let lhs_string = interp.scoped_value(scope, Value::string(lhs_string));
                let rhs_value = interp.escape_scoped(rhs);
                let rhs_string = interp.js_string_for_concat(rhs_value)?;
                let rhs_string = interp.scoped_value(scope, Value::string(rhs_string));
                let lhs_string = interp
                    .escape_scoped(lhs_string)
                    .as_string(&interp.gc_heap)
                    .ok_or(VmError::TypeMismatch)?;
                let rhs_string = interp
                    .escape_scoped(rhs_string)
                    .as_string(&interp.gc_heap)
                    .ok_or(VmError::TypeMismatch)?;
                Ok(Value::string(JsString::concat(
                    lhs_string,
                    rhs_string,
                    interp.gc_heap_mut(),
                )?))
            } else {
                let lhs_numeric = abstract_ops::to_numeric_kind(&lhs_value, &interp.gc_heap)
                    .ok_or(VmError::TypeMismatch)?;
                let rhs_numeric = abstract_ops::to_numeric_kind(&rhs_value, &interp.gc_heap)
                    .ok_or(VmError::TypeMismatch)?;
                match (lhs_numeric, rhs_numeric) {
                    (abstract_ops::NumericKind::Num(a), abstract_ops::NumericKind::Num(b)) => {
                        Ok(Value::number(number::add(a, b)))
                    }
                    (abstract_ops::NumericKind::Big(a), abstract_ops::NumericKind::Big(b)) => {
                        let sum = bigint::ops::add(&a, &b);
                        let handle = bigint::BigIntValue::from_inner(&mut interp.gc_heap, sum)
                            .map_err(oom_to_vm)?;
                        Ok(Value::big_int(handle))
                    }
                    // §6.1.6.2 Numeric Type Conversion forbids mixing
                    // Number and BigInt operands without an explicit coercion.
                    (abstract_ops::NumericKind::Num(_), abstract_ops::NumericKind::Big(_))
                    | (abstract_ops::NumericKind::Big(_), abstract_ops::NumericKind::Num(_)) => {
                        Err(VmError::TypeMismatch)
                    }
                }
            }
        })
    }

    pub(crate) fn run_compare_regs(
        &mut self,
        stack: &mut ActivationStack,
        context: &crate::execution_context::ExecutionContext,
        top_idx: usize,
        dst: u16,
        lhs: u16,
        rhs: u16,
        op: Op,
        feedback: Option<InstructionFeedbackRecorder<'_>>,
    ) -> Result<(), VmError> {
        let (dst, lhs, rhs) = binop_values(&stack[top_idx], dst, lhs, rhs)?;
        if let Some(feedback) = feedback {
            feedback.record_arith(lhs, rhs);
        }
        // Number vs Number is the dominant shape and needs neither the Symbol
        // rejection nor the String / BigInt / ToNumeric ladder. IEEE-754
        // comparison already yields the spec answer for every operator,
        // including the `false` an unordered NaN operand produces and the
        // equality of `+0` and `-0`.
        if let (Some(a), Some(b)) = (lhs.as_number(), rhs.as_number()) {
            let (a, b) = (a.as_f64(), b.as_f64());
            let truthy = match op {
                Op::LessThan => a < b,
                Op::GreaterThan => a > b,
                Op::LessEq => a <= b,
                Op::GreaterEq => a >= b,
                _ => return Err(VmError::InvalidOperand),
            };
            return commit_frame_result(&mut stack[top_idx], dst, Value::boolean(truthy));
        }
        let result = compare_value(self, stack, context, lhs, rhs, op)?;
        commit_frame_result(&mut stack[top_idx], dst, result)
    }

    pub(crate) fn run_ushr_regs(
        &mut self,
        stack: &mut ActivationStack,
        context: &crate::execution_context::ExecutionContext,
        top_idx: usize,
        dst: u16,
        lhs: u16,
        rhs: u16,
        feedback: Option<InstructionFeedbackRecorder<'_>>,
    ) -> Result<(), VmError> {
        let (dst, lhs, rhs) = binop_values(&stack[top_idx], dst, lhs, rhs)?;
        if let Some(feedback) = feedback {
            feedback.record_arith(lhs, rhs);
        }
        let result = ushr_value(self, stack, context, lhs, rhs)?;
        commit_frame_result(&mut stack[top_idx], dst, result)
    }

    pub(crate) fn run_neg_regs(
        &mut self,
        frame: &mut Frame,
        dst: u16,
        src: u16,
        feedback: Option<InstructionFeedbackRecorder<'_>>,
    ) -> Result<(), VmError> {
        let value = *read_register(frame, src)?;
        if let Some(feedback) = feedback {
            feedback.record_arith(value, value);
        }
        let result = self.neg_value(value)?;
        commit_frame_result(frame, dst, result)
    }

    pub(crate) fn run_bitwise_not_regs(
        &mut self,
        frame: &mut Frame,
        dst: u16,
        src: u16,
    ) -> Result<(), VmError> {
        let value = *read_register(frame, src)?;
        let result = self.bitwise_not_value(value)?;
        commit_frame_result(frame, dst, result)
    }

    /// Execute one update-expression numeric step through the same semantic
    /// path used by interpreter dispatch and compiled numeric slow paths.
    /// Observable coercion completes before `dst` is committed.
    pub(crate) fn run_increment_regs(
        &mut self,
        stack: &mut ActivationStack,
        context: &crate::execution_context::ExecutionContext,
        top_idx: usize,
        dst: u16,
        src: u16,
        delta: i32,
        feedback: Option<InstructionFeedbackRecorder<'_>>,
    ) -> Result<(), VmError> {
        let value = *read_register(&stack[top_idx], src)?;
        if let Some(feedback) = feedback {
            feedback.record_arith(value, Value::number_i32(delta));
        }
        let result = self.increment_value(stack, context, value, delta)?;
        commit_frame_result(&mut stack[top_idx], dst, result)
    }

    /// Evaluate one decoded native numeric request from already-read values.
    ///
    /// `rhs` is required exactly for variants carrying a right-hand register.
    /// The operation returns its value without advancing or otherwise mutating
    /// frame state, so materialized and native register windows share it.
    pub(crate) fn numeric_runtime_value(
        &mut self,
        stack: &mut ActivationStack,
        context: &crate::execution_context::ExecutionContext,
        operation: NumericRuntimeOp,
        lhs: Value,
        rhs: Option<Value>,
    ) -> Result<Value, VmError> {
        let binary_rhs = || rhs.ok_or(VmError::InvalidOperand);
        match operation {
            NumericRuntimeOp::Sub { .. } => numeric_binary_value(
                self,
                stack,
                context,
                lhs,
                binary_rhs()?,
                number::sub,
                bigint_sub_op,
            ),
            NumericRuntimeOp::Mul { .. } => numeric_binary_value(
                self,
                stack,
                context,
                lhs,
                binary_rhs()?,
                number::mul,
                bigint_mul_op,
            ),
            NumericRuntimeOp::Div { .. } => numeric_binary_value(
                self,
                stack,
                context,
                lhs,
                binary_rhs()?,
                number::div,
                bigint::ops::div,
            ),
            NumericRuntimeOp::Rem { .. } => numeric_binary_value(
                self,
                stack,
                context,
                lhs,
                binary_rhs()?,
                number::rem,
                bigint::ops::rem,
            ),
            NumericRuntimeOp::Pow { .. } => numeric_binary_value(
                self,
                stack,
                context,
                lhs,
                binary_rhs()?,
                number::pow,
                bigint::ops::pow,
            ),
            NumericRuntimeOp::BitwiseAnd { .. } => numeric_binary_value(
                self,
                stack,
                context,
                lhs,
                binary_rhs()?,
                number::bitwise_and,
                bigint_and_op,
            ),
            NumericRuntimeOp::BitwiseOr { .. } => numeric_binary_value(
                self,
                stack,
                context,
                lhs,
                binary_rhs()?,
                number::bitwise_or,
                bigint_or_op,
            ),
            NumericRuntimeOp::BitwiseXor { .. } => numeric_binary_value(
                self,
                stack,
                context,
                lhs,
                binary_rhs()?,
                number::bitwise_xor,
                bigint_xor_op,
            ),
            NumericRuntimeOp::Shl { .. } => numeric_binary_value(
                self,
                stack,
                context,
                lhs,
                binary_rhs()?,
                number::shl,
                bigint::ops::shl,
            ),
            NumericRuntimeOp::Shr { .. } => numeric_binary_value(
                self,
                stack,
                context,
                lhs,
                binary_rhs()?,
                number::shr_arith,
                bigint::ops::shr,
            ),
            NumericRuntimeOp::Ushr { .. } => ushr_value(self, stack, context, lhs, binary_rhs()?),
            NumericRuntimeOp::LessThan { .. } => {
                compare_value(self, stack, context, lhs, binary_rhs()?, Op::LessThan)
            }
            NumericRuntimeOp::LessEq { .. } => {
                compare_value(self, stack, context, lhs, binary_rhs()?, Op::LessEq)
            }
            NumericRuntimeOp::GreaterThan { .. } => {
                compare_value(self, stack, context, lhs, binary_rhs()?, Op::GreaterThan)
            }
            NumericRuntimeOp::GreaterEq { .. } => {
                compare_value(self, stack, context, lhs, binary_rhs()?, Op::GreaterEq)
            }
            NumericRuntimeOp::Neg => self.neg_value(lhs),
            NumericRuntimeOp::BitwiseNot => self.bitwise_not_value(lhs),
            NumericRuntimeOp::Increment { delta } => {
                self.increment_value(stack, context, lhs, delta)
            }
        }
    }

    pub(crate) fn neg_value(&mut self, value: Value) -> Result<Value, VmError> {
        match abstract_ops::to_numeric_kind(&value, &self.gc_heap).ok_or(VmError::TypeMismatch)? {
            abstract_ops::NumericKind::Num(number_value) => {
                Ok(Value::number(number::neg(number_value)))
            }
            abstract_ops::NumericKind::Big(big) => {
                let negated = bigint::ops::neg(&big);
                let handle = bigint::BigIntValue::from_inner(&mut self.gc_heap, negated)
                    .map_err(oom_to_vm)?;
                Ok(Value::big_int(handle))
            }
        }
    }

    fn bitwise_not_value(&mut self, value: Value) -> Result<Value, VmError> {
        match abstract_ops::to_numeric_kind(&value, &self.gc_heap).ok_or(VmError::TypeMismatch)? {
            abstract_ops::NumericKind::Num(number_value) => {
                Ok(Value::number(number::bitwise_not(number_value)))
            }
            abstract_ops::NumericKind::Big(big) => {
                let inverted = bigint::ops::bitwise_not(&big);
                let handle = bigint::BigIntValue::from_inner(&mut self.gc_heap, inverted)
                    .map_err(oom_to_vm)?;
                Ok(Value::big_int(handle))
            }
        }
    }

    fn increment_value(
        &mut self,
        stack: &mut ActivationStack,
        context: &crate::execution_context::ExecutionContext,
        value: Value,
        delta: i32,
    ) -> Result<Value, VmError> {
        let primitive = self.evaluate_to_primitive(
            stack,
            context,
            &value,
            abstract_ops::ToPrimitiveHint::Number,
        )?;
        let kind = abstract_ops::to_numeric_kind(&primitive, &self.gc_heap)
            .ok_or(VmError::TypeMismatch)?;
        match kind {
            abstract_ops::NumericKind::Num(number_value) => Ok(Value::number(
                NumberValue::from_f64(number_value.as_f64() + f64::from(delta)),
            )),
            abstract_ops::NumericKind::Big(big) => {
                let delta_big = num_bigint::BigInt::from(delta);
                let sum = bigint::ops::add(&big, &delta_big);
                let handle = bigint::BigIntValue::from_inner(&mut self.gc_heap, sum)
                    .map_err(|_| VmError::TypeMismatch)?;
                Ok(Value::big_int(handle))
            }
        }
    }

    pub(crate) fn run_equal_regs(
        &mut self,
        frame: &mut Frame,
        dst: u16,
        lhs: u16,
        rhs: u16,
        negate: bool,
        feedback: Option<InstructionFeedbackRecorder<'_>>,
    ) -> Result<(), VmError> {
        let (dst, lhs, rhs) = binop_values(frame, dst, lhs, rhs)?;
        // Record operand-type feedback like the relational path: a `===` / `!==`
        // between two numbers is numeric equality, so the optimizing tier can
        // speculate an int32 / float compare (the operand guards deopt a
        // mismatched type). Without this a strict-equality site stays unfed and
        // declines at tier-up.
        if let Some(feedback) = feedback {
            feedback.record_arith(lhs, rhs);
        }
        let eq = abstract_ops::is_strictly_equal(&lhs, &rhs, &self.gc_heap);
        write_register(frame, dst, Value::boolean(eq ^ negate))?;
        frame.advance_pc()?;
        Ok(())
    }

    pub(crate) fn run_loose_equal_regs(
        &mut self,
        stack: &mut ActivationStack,
        context: &crate::execution_context::ExecutionContext,
        top_idx: usize,
        dst: u16,
        lhs: u16,
        rhs: u16,
        negate: bool,
        feedback: Option<InstructionFeedbackRecorder<'_>>,
    ) -> Result<(), VmError> {
        let (dst, lhs, rhs) = binop_values(&stack[top_idx], dst, lhs, rhs)?;
        if let Some(feedback) = feedback {
            feedback.record_arith(lhs, rhs);
        }
        let eq = self.loose_equal_with_context(stack, context, &lhs, &rhs)?;
        write_register(&mut stack[top_idx], dst, Value::boolean(eq ^ negate))?;
        stack[top_idx].advance_pc()?;
        Ok(())
    }

    /// §7.2.13 `IsLooselyEqual(x, y)` with full access to the
    /// interpreter so the object × primitive ToPrimitive arm can
    /// invoke user-defined `@@toPrimitive` / `valueOf` / `toString`
    /// per §7.1.1. Two object operands compare via reference
    /// identity (§7.2.13 step 1 + IsStrictlyEqual for objects).
    pub(crate) fn loose_equal_with_context(
        &mut self,
        stack: &mut ActivationStack,
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
        // §7.2.13 step 11-12 — Object × primitive: ToPrimitive the object
        // operand with the `default` hint, then recurse over the resulting
        // primitive pair. Steps 11-12 only fire when the primitive is a
        // Number, String, BigInt, or Symbol — an Object compared against
        // `null` or `undefined` falls through to step 13 and is never equal,
        // so it must NOT coerce the object (which would observe `valueOf`).
        let (object, primitive, object_is_x) = if !abstract_ops::is_primitive(x) {
            (x, y, true)
        } else {
            (y, x, false)
        };
        if primitive.is_null() || primitive.is_undefined() {
            return Ok(false);
        }
        let coerced = self.evaluate_to_primitive(
            stack,
            context,
            object,
            abstract_ops::ToPrimitiveHint::Default,
        )?;
        let (lhs_p, rhs_p) = if object_is_x {
            (coerced, *y)
        } else {
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
        frame.advance_pc()?;
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

/// §13.15.3 ApplyStringOrNumericBinaryOperator steps 3-4 for the non-additive
/// numeric, bitwise, and shift operators.
///
/// The operands arrive exactly as the expression produced them. `ToNumeric`
/// over the left operand completes — including any `valueOf` /
/// `[Symbol.toPrimitive]` it runs — before the right operand's begins, so the
/// observable conversion order is the spec's.
fn numeric_binary_value(
    interp: &mut Interpreter,
    stack: &mut ActivationStack,
    context: &crate::execution_context::ExecutionContext,
    lhs: Value,
    rhs: Value,
    op: fn(NumberValue, NumberValue) -> NumberValue,
    bigint_op: BigIntBinop,
) -> Result<Value, VmError> {
    // Two numbers are already their own `ToNumeric` result; folding them here
    // skips building and matching two `NumericKind` values that each carry a
    // BigInt payload.
    if let (Some(a), Some(b)) = (lhs.as_number(), rhs.as_number()) {
        return Ok(Value::number(op(a, b)));
    }
    let (lhs, rhs) = interp.coerce_numeric_operands(stack, context, lhs, rhs)?;
    let lnum =
        abstract_ops::to_numeric_kind(&lhs, interp.gc_heap()).ok_or(VmError::TypeMismatch)?;
    let rnum =
        abstract_ops::to_numeric_kind(&rhs, interp.gc_heap()).ok_or(VmError::TypeMismatch)?;
    match (lnum, rnum) {
        (abstract_ops::NumericKind::Num(a), abstract_ops::NumericKind::Num(b)) => {
            Ok(Value::number(op(a, b)))
        }
        (abstract_ops::NumericKind::Big(a), abstract_ops::NumericKind::Big(b)) => {
            let folded = bigint_op(&a, &b).map_err(|err| bigint_to_vm_error(interp, err))?;
            let handle =
                bigint::BigIntValue::from_inner(interp.gc_heap_mut(), folded).map_err(oom_to_vm)?;
            Ok(Value::big_int(handle))
        }
        _ => Err(VmError::TypeMismatch),
    }
}

fn ushr_value(
    interp: &mut Interpreter,
    stack: &mut ActivationStack,
    context: &crate::execution_context::ExecutionContext,
    lhs: Value,
    rhs: Value,
) -> Result<Value, VmError> {
    let (lhs, rhs) = interp.coerce_numeric_operands(stack, context, lhs, rhs)?;
    let heap = &interp.gc_heap;
    let lhs = abstract_ops::to_numeric_kind(&lhs, heap).ok_or(VmError::TypeMismatch)?;
    let rhs = abstract_ops::to_numeric_kind(&rhs, heap).ok_or(VmError::TypeMismatch)?;
    match (lhs, rhs) {
        (abstract_ops::NumericKind::Num(a), abstract_ops::NumericKind::Num(b)) => {
            Ok(Value::number(number::shr_logical(a, b)))
        }
        _ => Err(VmError::TypeMismatch),
    }
}

/// §7.2.14 IsLessThan — the relational operators, including their
/// `ToPrimitive(number)` step over each operand.
///
/// Step 1 orders the two conversions left-to-right, and a `valueOf` /
/// `[Symbol.toPrimitive]` on the left operand is fully observed before the
/// right operand is touched.
fn compare_value(
    interp: &mut Interpreter,
    stack: &mut ActivationStack,
    context: &crate::execution_context::ExecutionContext,
    lhs: Value,
    rhs: Value,
    op: Op,
) -> Result<Value, VmError> {
    let (lhs, rhs) = interp.coerce_operand_pair(
        stack,
        context,
        lhs,
        rhs,
        abstract_ops::ToPrimitiveHint::Number,
    )?;
    let heap = &interp.gc_heap;
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
    Ok(Value::boolean(truthy))
}

fn commit_frame_result(frame: &mut Frame, dst: u16, result: Value) -> Result<(), VmError> {
    write_register(frame, dst, result)?;
    frame.advance_pc()?;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_context() -> crate::ExecutionContext {
        crate::ExecutionContext::from_module(crate::BytecodeModule {
            module: "numeric-runtime-test.js".to_string(),
            template_sites: Vec::new(),
            source_kind: otter_bytecode::SourceKind::TypeScript,
            functions: Vec::new(),
            constants: Vec::new(),
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        })
    }

    #[test]
    fn numeric_runtime_op_decodes_binary_and_signed_update_operands() {
        assert_eq!(
            NumericRuntimeOp::decode_abi(Op::Sub as u64, 41).expect("binary opcode"),
            NumericRuntimeOp::Sub { rhs: 41 }
        );
        assert_eq!(
            NumericRuntimeOp::decode_abi(Op::Increment as u64, u32::MAX.into())
                .expect("signed update opcode"),
            NumericRuntimeOp::Increment { delta: -1 }
        );
    }

    #[test]
    fn numeric_runtime_op_rejects_non_numeric_and_unrepresentable_operands() {
        assert!(matches!(
            NumericRuntimeOp::decode_abi(Op::Add as u64, 0),
            Err(VmError::InvalidOperand)
        ));
        assert!(matches!(
            NumericRuntimeOp::decode_abi(Op::Sub as u64, u64::from(u16::MAX) + 1),
            Err(VmError::InvalidOperand)
        ));
        assert!(matches!(
            NumericRuntimeOp::decode_abi(u64::from(u8::MAX) + 1, 0),
            Err(VmError::InvalidOperand)
        ));
    }

    #[test]
    fn typed_numeric_runtime_dispatch_returns_values_without_frame_state() {
        let context = empty_context();
        let mut interp = Interpreter::new();
        let mut stack = ActivationStack::new();

        let difference = interp
            .numeric_runtime_value(
                &mut stack,
                &context,
                NumericRuntimeOp::Sub { rhs: 1 },
                Value::number_i32(9),
                Some(Value::number_i32(4)),
            )
            .expect("number subtraction");
        assert_eq!(difference, Value::number_i32(5));

        let comparison = interp
            .numeric_runtime_value(
                &mut stack,
                &context,
                NumericRuntimeOp::LessThan { rhs: 1 },
                Value::number_i32(4),
                Some(Value::number_i32(9)),
            )
            .expect("number comparison");
        assert_eq!(comparison, Value::boolean(true));

        assert!(matches!(
            interp.numeric_runtime_value(
                &mut stack,
                &context,
                NumericRuntimeOp::Mul { rhs: 1 },
                Value::number_i32(3),
                None,
            ),
            Err(VmError::InvalidOperand)
        ));
    }

    #[test]
    fn add_kernel_roots_string_operands_without_frame_storage() {
        let mut interp = Interpreter::new();
        let lhs_text = "rooted-string-value-that-is-longer-than-inline-";
        let lhs = Value::string(
            JsString::from_str(lhs_text, interp.gc_heap_mut()).expect("left string allocation"),
        );
        let mut stack = crate::ActivationStack::new();
        let context =
            crate::execution_context::ExecutionContext::from_module(crate::BytecodeModule {
                module: "add-kernel-rooting-test.js".to_string(),
                template_sites: Vec::new(),
                source_kind: otter_bytecode::SourceKind::TypeScript,
                functions: Vec::new(),
                constants: Vec::new(),
                module_resolutions: Vec::new(),
                module_inits: Vec::new(),
            });
        let result = interp
            .add_value(&mut stack, &context, lhs, Value::number_i32(7))
            .expect("string addition");
        let text = result
            .as_string(interp.gc_heap())
            .expect("string result")
            .to_lossy_string(interp.gc_heap());
        assert_eq!(text, format!("{lhs_text}7"));
    }
}
