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
    activation_stack::ActivationStack, arithmetic_dispatch::NumericRuntimeOp,
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
        let result = self.numeric_runtime_value(context, operation, lhs, rhs)?;
        frame.write(dst, result)
    }

    /// Execute generic ECMAScript addition against the canonical activation.
    pub fn jit_runtime_add(
        &mut self,
        frame: &mut ActiveFrameMut<'_>,
        dst: u16,
        lhs: u16,
        rhs: u16,
    ) -> Result<(), VmError> {
        self.record_jit_runtime_stub_class(crate::native_abi::RuntimeStubClass::Reentrant);
        let lhs = frame.read(lhs)?;
        let rhs = frame.read(rhs)?;
        let result = self.add_value(lhs, rhs)?;
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
        context: &ExecutionContext,
        frame: &mut ActiveFrameMut<'_>,
        dst: u16,
        src: u16,
        operation: UnaryCoercionOp,
    ) -> Result<(), VmError> {
        self.record_jit_runtime_stub_class(crate::native_abi::RuntimeStubClass::Reentrant);
        let input = frame.read(src)?;
        let result = self.coerce_unary_value(context, input, operation)?;
        frame.write(dst, result)
    }

    /// Evaluate one typed coercive unary operation independently of frame
    /// storage. The returned value is ready for an immediate destination
    /// commit and no source/destination register aliasing assumptions leak in.
    pub(crate) fn coerce_unary_value(
        &mut self,
        context: &ExecutionContext,
        input: Value,
        operation: UnaryCoercionOp,
    ) -> Result<Value, VmError> {
        self.with_handle_scope(|interp, scope| {
            let input = interp.scoped_value(scope, input);
            let (numeric, hint) = match operation {
                UnaryCoercionOp::ToNumeric => (true, abstract_ops::ToPrimitiveHint::Number),
                UnaryCoercionOp::ToPrimitive { hint } => (false, hint.abstract_hint()),
            };
            let current = interp.escape_scoped(input);
            let primitive = if abstract_ops::is_primitive(&current) {
                current
            } else {
                interp.evaluate_to_primitive(context, &current, hint)?
            };
            let primitive = interp.scoped_value(scope, primitive);
            let primitive_value = interp.escape_scoped(primitive);
            let result = if !numeric || primitive_value.is_number() || primitive_value.is_big_int()
            {
                primitive_value
            } else if primitive_value.is_symbol() {
                return Err(interp
                    .err_type(("Cannot convert a Symbol value to a number".to_string()).into()));
            } else {
                Value::number(crate::number::NumberValue::from_f64(
                    crate::number::parse::to_number_value(&primitive_value, &interp.gc_heap),
                ))
            };
            let result = interp.scoped_value(scope, result);
            Ok(interp.escape_scoped(result))
        })
    }

    /// Store a captured binding after enforcing its TDZ check.
    pub fn jit_runtime_store_upvalue_checked(
        &mut self,
        frame: &mut ActiveFrameMut<'_>,
        src: u16,
        idx: i32,
    ) -> Result<(), VmError> {
        self.frame_store_upvalue_checked(frame, src, idx)
    }

    /// Materialize a string constant from the owning function's constant pool.
    pub fn jit_runtime_load_string(
        &mut self,
        context: &ExecutionContext,
        frame: &mut ActiveFrameMut<'_>,
        function_id: u32,
        dst: u16,
        constant_index: u32,
    ) -> Result<(), VmError> {
        let resolved = context
            .for_function(function_id)
            .ok_or(VmError::InvalidOperand)?;
        let value = self.load_string_constant_value(&resolved, constant_index)?;
        frame.write(dst, value)
    }

    /// Define one object-literal data property from decoded registers.
    pub fn jit_runtime_define_data_property(
        &mut self,
        context: &ExecutionContext,
        frame: &mut ActiveFrameMut<'_>,
        object: u16,
        key: u16,
        value: u16,
    ) -> Result<(), VmError> {
        self.run_define_data_property_active(context, frame, object, key, value)
    }

    /// Replace one loop-captured upvalue cell with a fresh TDZ cell.
    pub fn jit_runtime_fresh_upvalue(
        &mut self,
        frame: &mut ActiveFrameMut<'_>,
        idx: i32,
    ) -> Result<(), VmError> {
        self.frame_fresh_upvalue(frame, idx)
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
            .ok_or(VmError::InvalidOperand)?;
        let saved_pc = frame.pc();
        let result = self.run_load_builtin_error_active(&resolved, frame, dst, kind_index);
        frame.set_pc(saved_pc);
        result
    }

    /// Execute generic ECMAScript unary negation against the canonical frame.
    pub fn jit_runtime_neg(
        &mut self,
        frame: &mut ActiveFrameMut<'_>,
        dst: u16,
        src: u16,
    ) -> Result<(), VmError> {
        self.record_jit_runtime_stub_class(crate::native_abi::RuntimeStubClass::Reentrant);
        let value = frame.read(src)?;
        let result = self.neg_value(value)?;
        frame.write(dst, result)
    }

    /// Apply a descriptor object through `OrdinaryDefineOwnProperty`.
    pub fn jit_runtime_define_own_property(
        &mut self,
        context: &ExecutionContext,
        frame: &mut ActiveFrameMut<'_>,
        target: u16,
        key: u16,
        descriptor: u16,
    ) -> Result<(), VmError> {
        self.run_define_own_property_active(context, frame, target, key, descriptor)
    }

    /// Allocate a closure from decoded function and parent-upvalue indices.
    pub fn jit_runtime_make_closure(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        frame_index: usize,
        function_id: u32,
        dst: u16,
        function_index: u32,
        parent_indices: &[u32],
    ) -> Result<(), VmError> {
        let resolved = context
            .for_function(function_id)
            .ok_or(VmError::InvalidOperand)?;
        let saved_pc = stack[frame_index].pc;
        let result = self.run_make_closure_regs(
            &resolved,
            &mut stack[frame_index],
            dst,
            function_index,
            parent_indices,
        );
        stack[frame_index].pc = saved_pc;
        result
    }

    /// Allocate a closure directly from a published frameless native owner.
    ///
    /// Direct-call eligibility excludes cold eval/constructor state, so the
    /// canonical native SELF/`this`/upvalue windows contain the complete source
    /// state and no interpreter [`Frame`] adapter is required.
    pub fn jit_runtime_make_closure_native(
        &mut self,
        context: &ExecutionContext,
        frame: &mut ActiveFrameMut<'_>,
        function_id: u32,
        dst: u16,
        function_index: u32,
        parent_indices: &[u32],
    ) -> Result<(), VmError> {
        if frame.function_id() != function_id {
            return Err(VmError::InvalidOperand);
        }
        let resolved = context
            .for_function(function_id)
            .ok_or(VmError::InvalidOperand)?;
        let saved_pc = frame.pc();
        let result = self.run_make_closure_active_regs(
            &resolved,
            frame,
            dst,
            function_index,
            parent_indices,
            None,
            None,
            None,
        );
        frame.set_pc(saved_pc);
        result
    }

    /// Allocate a distinct capture-free function value directly in a
    /// published frameless native owner.
    ///
    /// Direct-call eligibility excludes direct-eval cold state, while the
    /// native descriptor publishes the exact SELF value needed by named
    /// self-references. The compiled PC remains owned by generated code.
    pub fn jit_runtime_make_function_native(
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
            .ok_or(VmError::InvalidOperand)?;
        let saved_pc = frame.pc();
        let result = self.run_make_function_active_reg(&resolved, frame, dst, function_index, None);
        frame.set_pc(saved_pc);
        result
    }

    /// Execute a guarded `Math` call from decoded argument registers.
    #[allow(clippy::too_many_arguments)]
    pub fn jit_runtime_math_call(
        &mut self,
        context: &ExecutionContext,
        frame: &mut ActiveFrameMut<'_>,
        dst: u16,
        method_id: u32,
        argument_regs: &[u16],
    ) -> Result<(), VmError> {
        self.do_math_call_active(context, frame, dst, method_id, argument_regs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native_abi::{NativeFrame, NativeFrameFlags, NativeFrameKind, VmFrameHeader};

    fn empty_context() -> ExecutionContext {
        ExecutionContext::from_module(crate::BytecodeModule {
            module: "jit-numeric-native-frame-test.js".to_string(),
            template_sites: Vec::new(),
            source_kind: otter_bytecode::SourceKind::TypeScript,
            functions: Vec::new(),
            constants: Vec::new(),
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        })
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
        let primitive = interp
            .coerce_unary_value(
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
            code_block_id: 7,
            pc: 19,
            register_count: registers.len() as u16,
            kind: NativeFrameKind::Baseline,
            flags: NativeFrameFlags::empty(),
        };
        let mut native = NativeFrame::new(
            header,
            registers.as_mut_ptr() as u64,
            crate::Value::undefined(),
            crate::Value::undefined(),
        );
        {
            // SAFETY: `native` and its initialized register array remain live
            // and unmoved for the active view's scoped lifetime.
            let mut frame = unsafe { ActiveFrameMut::from_native_ptr(&mut native) }
                .expect("valid native activation");
            let mut interp = Interpreter::new();
            let context = empty_context();

            interp
                .jit_runtime_numeric_op(
                    &context,
                    &mut frame,
                    2,
                    0,
                    NumericRuntimeOp::Sub { rhs: 1 },
                )
                .expect("native numeric runtime op");
            interp
                .jit_runtime_add(&mut frame, 3, 0, 1)
                .expect("native add runtime op");
            interp
                .jit_runtime_neg(&mut frame, 4, 1)
                .expect("native negate runtime op");
            interp
                .jit_runtime_coerce_unary(&context, &mut frame, 6, 5, UnaryCoercionOp::ToNumeric)
                .expect("native ToNumeric runtime op");
        }

        assert_eq!(registers[2].as_f64(), Some(5.0));
        assert_eq!(registers[3].as_f64(), Some(13.0));
        assert_eq!(registers[4].as_f64(), Some(-4.0));
        assert_eq!(registers[6].as_f64(), Some(1.0));
        assert_eq!(native.header.pc, 19);
    }
}
