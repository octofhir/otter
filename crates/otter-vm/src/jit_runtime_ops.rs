//! Typed runtime operations used by baseline JIT slow paths.
//!
//! # Contents
//! - [`UnaryCoercionOp`] / [`UnaryPrimitiveHint`] — fully decoded coercion
//!   intent with no constant-pool metadata in the semantic kernel.
//! - Fixed-operand operations whose full ECMAScript semantics still belong to
//!   the VM: arithmetic, coercion, captured-binding checks, constant
//!   materialization, descriptor writes, and loose equality.
//!
//! # Invariants
//! - Every operand is decoded by the compiler and passed explicitly. These
//!   functions never receive a byte PC or decode a `CodeBlockInstruction`.
//! - Arithmetic and unary-coercion semantics consume typed values through a
//!   representation-neutral [`ActiveFrameMut`]; no ActivationStack identity or raw
//!   register pointer enters those paths.
//! - Published active-frame slots remain the canonical moving-GC roots across
//!   allocating or throwing operations.
//! - The compiled frame's instruction PC is preserved; advancing dispatch is
//!   the interpreter caller's responsibility, not the JIT ABI's.
//!
//! # See also
//! - `crate::property_dispatch` for typed property and element slow paths.
//! - `otter-jit::template` for the machine-code stubs calling these operations.

use crate::{
    ActiveFrameMut, ExecutionContext, Interpreter, Value, VmError, abstract_ops,
    arithmetic_dispatch::NumericRuntimeOp,
};

/// Fully decoded `ToPrimitive` hint used by unary-coercion semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryPrimitiveHint {
    /// ECMAScript `default` hint.
    Default,
    /// ECMAScript `number` hint.
    Number,
    /// ECMAScript `string` hint.
    String,
}

impl UnaryPrimitiveHint {
    /// Decode a compiler-owned hint token after the ABI adapter resolves it
    /// through the canonical frame's function identity.
    #[must_use]
    pub fn from_token(token: &str) -> Option<Self> {
        match token {
            "default" => Some(Self::Default),
            "number" => Some(Self::Number),
            "string" => Some(Self::String),
            _ => None,
        }
    }

    fn abstract_hint(self) -> abstract_ops::ToPrimitiveHint {
        match self {
            Self::Default => abstract_ops::ToPrimitiveHint::Default,
            Self::Number => abstract_ops::ToPrimitiveHint::Number,
            Self::String => abstract_ops::ToPrimitiveHint::String,
        }
    }
}

/// Fully decoded coercive unary operation requested by native code.
///
/// Raw ABI mode words, function ownership, and hint constant identities are
/// consumed by the JIT entry before this value crosses into VM semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryCoercionOp {
    /// ECMAScript `ToPrimitive` with a resolved semantic hint.
    ToPrimitive {
        /// Already-resolved `preferredType` semantic hint.
        hint: UnaryPrimitiveHint,
    },
    /// ECMAScript `ToNumeric` (`ToPrimitive(number)` plus numeric conversion).
    ToNumeric,
}

impl Interpreter {
    /// Complete one decoded numeric request against the published active frame.
    ///
    /// Semantics return a value before the destination is committed. Native
    /// progress is intentionally unchanged: generated code, not this runtime
    /// operation, owns the compiled instruction PC.
    pub fn jit_runtime_numeric_op(
        &mut self,
        stack: &mut crate::ActivationStack,
        context: &ExecutionContext,
        frame: &mut ActiveFrameMut<'_>,
        dst: u16,
        lhs: u16,
        operation: NumericRuntimeOp,
    ) -> Result<(), VmError> {
        self.record_jit_runtime_stub_class(crate::native_abi::RuntimeStubClass::Reentrant);
        let lhs = frame.read(lhs)?;
        let rhs = operation
            .rhs_register()
            .map(|register| frame.read(register))
            .transpose()?;
        record_completed_arith(context, frame, lhs, rhs.unwrap_or(lhs));
        let result = self.numeric_runtime_value(stack, context, operation, lhs, rhs)?;
        frame.write(dst, result)
    }

    /// Execute generic ECMAScript addition against the canonical activation.
    pub fn jit_runtime_add(
        &mut self,
        stack: &mut crate::ActivationStack,
        context: &ExecutionContext,
        frame: &mut ActiveFrameMut<'_>,
        dst: u16,
        lhs: u16,
        rhs: u16,
    ) -> Result<(), VmError> {
        self.record_jit_runtime_stub_class(crate::native_abi::RuntimeStubClass::Reentrant);
        let lhs = frame.read(lhs)?;
        let rhs = frame.read(rhs)?;
        record_completed_arith(context, frame, lhs, rhs);
        let result = self.add_value(stack, context, lhs, rhs)?;
        frame.write(dst, result)
    }

    /// Complete a coercive unary operation against the canonical activation.
    ///
    /// The source is rooted in the handle arena before any user conversion
    /// hook can re-enter JavaScript. The destination is committed only after
    /// the complete abstract operation succeeds; compiled PC ownership stays
    /// with generated code. `ToPrimitive` arrives with a resolved semantic
    /// hint; constant-pool lookup is confined to the native ABI adapter.
    pub fn jit_runtime_coerce_unary(
        &mut self,
        stack: &mut crate::ActivationStack,
        context: &ExecutionContext,
        frame: &mut ActiveFrameMut<'_>,
        dst: u16,
        src: u16,
        operation: UnaryCoercionOp,
    ) -> Result<(), VmError> {
        self.record_jit_runtime_stub_class(crate::native_abi::RuntimeStubClass::Reentrant);
        let input = frame.read(src)?;
        let result = self.coerce_unary_value(stack, context, input, operation)?;
        frame.write(dst, result)
    }

    /// Evaluate one typed coercive unary operation independently of frame
    /// storage. The returned value is ready for an immediate destination
    /// commit and no source/destination register aliasing assumptions leak in.
    pub(crate) fn coerce_unary_value(
        &mut self,
        stack: &mut crate::ActivationStack,
        context: &ExecutionContext,
        input: Value,
        operation: UnaryCoercionOp,
    ) -> Result<Value, VmError> {
        match operation {
            UnaryCoercionOp::ToNumeric => {
                crate::coerce::to_numeric_or_throw(self, stack, context, &input)
            }
            UnaryCoercionOp::ToPrimitive { hint } => {
                self.evaluate_to_primitive(stack, context, &input, hint.abstract_hint())
            }
        }
    }

    /// Define one object-literal data property from decoded registers.
    pub fn jit_runtime_define_data_property(
        &mut self,
        stack: &mut crate::ActivationStack,
        context: &ExecutionContext,
        frame: &mut ActiveFrameMut<'_>,
        object: u16,
        key: u16,
        value: u16,
    ) -> Result<(), VmError> {
        self.run_define_data_property_active(stack, context, frame, object, key, value)
    }

    /// `CreateContext dst, parent, scope` over the published activation.
    ///
    /// # Errors
    /// Propagates allocation failure and `InvalidOperand` for an operand the
    /// verifier would reject.
    pub fn jit_runtime_create_context(
        &mut self,
        context: &ExecutionContext,
        frame: &mut ActiveFrameMut<'_>,
        dst: u16,
        parent: u16,
        scope_index: u32,
    ) -> Result<(), VmError> {
        let resolved = context
            .for_function(frame.function_id())
            .map_err(|_| VmError::InvalidOperand)?;
        self.frame_create_context(&resolved, frame, dst, parent, scope_index)
    }

    /// `CopyContext dst, src` over the published activation.
    ///
    /// # Errors
    /// Propagates allocation failure and `InvalidOperand`.
    pub fn jit_runtime_copy_context(
        &mut self,
        frame: &mut ActiveFrameMut<'_>,
        dst: u16,
        src: u16,
    ) -> Result<(), VmError> {
        self.frame_copy_context(frame, dst, src)
    }

    /// Load one realm builtin error constructor from a decoded constant index.
    pub fn jit_runtime_load_builtin_error(
        &self,
        context: &ExecutionContext,
        frame: &mut ActiveFrameMut<'_>,
        dst: u16,
        kind_index: u32,
    ) -> Result<(), VmError> {
        // `kind_index` is a constant-pool index of the COMPILED function's
        // chunk; in a multi-script runtime the ambient context may belong to
        // a different chunk, so resolve the owner before decoding.
        let function_id = frame.function_id();
        let resolved = context
            .for_function(function_id)
            .map_err(|_| VmError::InvalidOperand)?;
        let saved_pc = frame.pc();
        let result = self.run_load_builtin_error_active(&resolved, frame, dst, kind_index);
        frame.set_pc(saved_pc);
        result
    }

    /// Apply a descriptor object through `OrdinaryDefineOwnProperty`.
    pub fn jit_runtime_define_own_property(
        &mut self,
        stack: &mut crate::ActivationStack,
        context: &ExecutionContext,
        frame: &mut ActiveFrameMut<'_>,
        target: u16,
        key: u16,
        descriptor: u16,
    ) -> Result<(), VmError> {
        self.run_define_own_property_active(stack, context, frame, target, key, descriptor)
    }

    /// Allocate a closure directly from the published native frame.
    ///
    /// The closure closes over the context in `context_reg`; the canonical
    /// native `this` / `new.target` supply an arrow's lexical copies. No
    /// interpreter [`Frame`] adapter is required.
    pub fn jit_runtime_make_closure(
        &mut self,
        context: &ExecutionContext,
        frame: &mut ActiveFrameMut<'_>,
        function_id: u32,
        dst: u16,
        function_index: u32,
        context_reg: u16,
    ) -> Result<(), VmError> {
        if frame.function_id() != function_id {
            return Err(VmError::InvalidOperand);
        }
        let resolved = context
            .for_function(function_id)
            .map_err(|_| VmError::InvalidOperand)?;
        let saved_pc = frame.pc();
        // §10.2.1.1 — an arrow closes over the enclosing activation's
        // `new.target`. The published frame carries it for both native and
        // materialized activations; undefined is the unbound state.
        let new_target = frame.new_target_value();
        let lexical_new_target = (!new_target.is_undefined()).then_some(new_target);
        let result = self.run_make_closure_active_regs(
            &resolved,
            frame,
            dst,
            function_index,
            context_reg,
            lexical_new_target,
        );
        frame.set_pc(saved_pc);
        result
    }

    /// Allocate a distinct capture-free function value directly in a
    /// published stack-owned native frame.
    ///
    /// The native descriptor publishes the exact SELF value and direct-eval
    /// environment needed by nested function creation. The compiled PC remains
    /// owned by generated code.
    pub fn jit_runtime_make_function(
        &mut self,
        context: &ExecutionContext,
        frame: &mut ActiveFrameMut<'_>,
        function_id: u32,
        dst: u16,
        function_index: u32,
    ) -> Result<(), VmError> {
        if frame.function_id() != function_id {
            return Err(VmError::InvalidOperand);
        }
        let resolved = context
            .for_function(function_id)
            .map_err(|_| VmError::InvalidOperand)?;
        let saved_pc = frame.pc();
        let result = self.run_make_function_active_reg(&resolved, frame, dst, function_index);
        frame.set_pc(saved_pc);
        result
    }
}

/// Record the operands of a compiled arithmetic site completed here. Baseline
/// code reaches these completions with the representations its fast paths do
/// not handle, so the site's feedback stays complete without the fast paths
/// recording `int32`.
fn record_completed_arith(
    context: &ExecutionContext,
    frame: &ActiveFrameMut<'_>,
    lhs: crate::Value,
    rhs: crate::Value,
) {
    if let Some(recorder) = context
        .exec_function(frame.function_id())
        .and_then(|function| function.feedback_recorder_at(frame.pc() as usize))
    {
        recorder.record_arith(lhs, rhs);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native_abi::{Frame, NativeFrameFlags, NativeFrameKind, VmFrameHeader};

    fn empty_context() -> ExecutionContext {
        ExecutionContext::from_module(crate::test_support::minimal_bytecode_module(
            "jit-numeric-native-frame-test.js",
        ))
        .expect("valid bytecode fixture")
    }

    #[test]
    fn primitive_hint_tokens_are_resolved_before_vm_semantics() {
        assert_eq!(
            UnaryPrimitiveHint::from_token("default"),
            Some(UnaryPrimitiveHint::Default)
        );
        assert_eq!(
            UnaryPrimitiveHint::from_token("number"),
            Some(UnaryPrimitiveHint::Number)
        );
        assert_eq!(
            UnaryPrimitiveHint::from_token("string"),
            Some(UnaryPrimitiveHint::String)
        );
        assert_eq!(UnaryPrimitiveHint::from_token("invalid"), None);

        let mut interp = Interpreter::new();
        let mut stack = crate::ActivationStack::new();
        let primitive = interp
            .coerce_unary_value(
                &mut stack,
                &empty_context(),
                crate::Value::boolean(true),
                UnaryCoercionOp::ToPrimitive {
                    hint: UnaryPrimitiveHint::Default,
                },
            )
            .expect("primitive identity coercion");
        assert_eq!(primitive, crate::Value::boolean(true));
    }

    #[test]
    fn arithmetic_and_coercion_ops_commit_to_native_window_without_advancing_pc() {
        let mut registers = [
            crate::Value::number_i32(9),
            crate::Value::number_i32(4),
            crate::Value::undefined(),
            crate::Value::undefined(),
            crate::Value::undefined(),
            crate::Value::boolean(true),
            crate::Value::undefined(),
        ];
        let header = VmFrameHeader {
            function_id: 7,
            pc: 19,
            register_count: registers.len() as u16,
            kind: NativeFrameKind::Baseline,
            flags: NativeFrameFlags::empty(),
        };
        let mut native = Frame::new(
            header,
            registers.as_mut_ptr() as u64,
            crate::Value::undefined(),
            crate::Value::undefined(),
        );
        {
            // SAFETY: `native` and its initialized register array remain live
            // and unmoved for the active view's scoped lifetime.
            let mut frame =
                unsafe { ActiveFrameMut::from_ptr(&mut native) }.expect("valid native activation");
            let mut interp = Interpreter::new();
            let mut stack = crate::ActivationStack::new();
            let context = empty_context();

            interp
                .jit_runtime_numeric_op(
                    &mut stack,
                    &context,
                    &mut frame,
                    2,
                    0,
                    NumericRuntimeOp::Sub { rhs: 1 },
                )
                .expect("native numeric runtime op");
            interp
                .jit_runtime_add(&mut stack, &context, &mut frame, 3, 0, 1)
                .expect("native add runtime op");
            interp
                .jit_runtime_numeric_op(
                    &mut stack,
                    &context,
                    &mut frame,
                    4,
                    1,
                    NumericRuntimeOp::Neg,
                )
                .expect("native negate runtime op");
            interp
                .jit_runtime_coerce_unary(
                    &mut stack,
                    &context,
                    &mut frame,
                    6,
                    5,
                    UnaryCoercionOp::ToNumeric,
                )
                .expect("native ToNumeric runtime op");
        }

        assert_eq!(registers[2].as_f64(), Some(5.0));
        assert_eq!(registers[3].as_f64(), Some(13.0));
        assert_eq!(registers[4].as_f64(), Some(-4.0));
        assert_eq!(registers[6].as_f64(), Some(1.0));
        assert_eq!(native.header.pc, 19);
    }
}
