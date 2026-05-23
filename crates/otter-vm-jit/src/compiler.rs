//! Baseline JIT compiler wrapper around Cranelift.

use cranelift_codegen::ir::{AbiParam, UserFuncName, types};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{Linkage, Module, ModuleError, default_libcall_names};
use otter_vm_bytecode::Function;

use crate::runtime_helpers::{HelperFuncIds, HelperRefs, RuntimeHelpers};
use crate::translator;

/// Result of compiling a bytecode function to native code.
#[derive(Debug, Clone, Copy)]
pub struct JitCompileArtifact {
    /// Entry pointer for compiled native code.
    pub code_ptr: *const u8,
}

/// Errors produced by the baseline JIT compiler.
#[derive(Debug, thiserror::Error)]
pub enum JitError {
    /// Cranelift module-level error.
    #[error("cranelift module error: {0}")]
    Module(Box<ModuleError>),

    /// Failed to create the JIT builder.
    #[error("jit builder initialization failed: {0}")]
    Builder(String),

    /// Bytecode instruction is not supported by the baseline translator yet.
    #[error("unsupported instruction at pc {pc}: {opcode}")]
    UnsupportedInstruction {
        /// Program counter of the unsupported instruction.
        pc: usize,
        /// Name of the unsupported opcode.
        opcode: String,
    },

    /// Jump target is outside the bytecode function bounds.
    #[error("invalid jump target from pc {pc} with offset {offset} (len={instruction_count})")]
    InvalidJumpTarget {
        /// Program counter of the jump instruction.
        pc: usize,
        /// Jump offset that was out of bounds.
        offset: i32,
        /// Total instruction count of the function.
        instruction_count: usize,
    },
}

impl From<ModuleError> for JitError {
    fn from(value: ModuleError) -> Self {
        Self::Module(Box::new(value))
    }
}

/// Minimal Cranelift-backed JIT compiler.
pub struct JitCompiler {
    module: JITModule,
    function_builder_ctx: FunctionBuilderContext,
    context: cranelift_codegen::Context,
    next_function_id: u64,
    helper_func_ids: HelperFuncIds,
}

impl JitCompiler {
    /// Create a new baseline JIT compiler without runtime helpers.
    ///
    /// Only pure-computation instructions can be compiled.
    pub fn new() -> Result<Self, JitError> {
        Self::with_helpers(RuntimeHelpers::new())
    }

    /// Create a JIT compiler with runtime helpers for complex operations.
    pub fn with_helpers(helpers: RuntimeHelpers) -> Result<Self, JitError> {
        let mut builder = JITBuilder::new(default_libcall_names())
            .map_err(|e| JitError::Builder(e.to_string()))?;
        helpers.register_symbols(&mut builder);
        let mut module = JITModule::new(builder);
        let helper_func_ids = HelperFuncIds::declare(&helpers, &mut module)?;
        Ok(Self {
            module,
            function_builder_ctx: FunctionBuilderContext::new(),
            context: cranelift_codegen::Context::new(),
            next_function_id: 0,
            helper_func_ids,
        })
    }

    /// Compile a bytecode function into native code.
    ///
    /// Compiled function signature: `extern "C" fn(ctx: *mut u8) -> i64`.
    pub fn compile_function(
        &mut self,
        function: &Function,
    ) -> Result<JitCompileArtifact, JitError> {
        let mut signature = self.module.make_signature();
        signature.params.push(AbiParam::new(types::I64)); // ctx pointer
        signature.returns.push(AbiParam::new(types::I64));

        let name = format!(
            "otter_jit_{}_{}",
            function.display_name().replace(['<', '>'], "_"),
            self.next_function_id
        );
        self.next_function_id = self.next_function_id.saturating_add(1);

        let func_id = self
            .module
            .declare_function(&name, Linkage::Local, &signature)?;

        self.context.func = cranelift_codegen::ir::Function::with_name_signature(
            UserFuncName::user(0, func_id.as_u32()),
            signature,
        );

        let helper_refs =
            HelperRefs::declare(&self.helper_func_ids, &mut self.module, &mut self.context.func);

        {
            let mut builder =
                FunctionBuilder::new(&mut self.context.func, &mut self.function_builder_ctx);
            translator::translate_function(&mut builder, function, &helper_refs)?;
            builder.finalize();
        }

        self.module.define_function(func_id, &mut self.context)?;
        self.module.clear_context(&mut self.context);
        self.module.finalize_definitions()?;

        let code_ptr = self.module.get_finalized_function(func_id);
        Ok(JitCompileArtifact { code_ptr })
    }

    /// Execute a compiled artifact with null context. For arithmetic-only tests.
    pub fn execute_compiled_i64(&self, artifact: JitCompileArtifact) -> i64 {
        // SAFETY: function has signature (i64) -> i64
        let func: extern "C" fn(i64) -> i64 =
            unsafe { std::mem::transmute(artifact.code_ptr) };
        func(0)
    }

    /// Execute a compiled artifact with a runtime context pointer.
    pub fn execute_with_ctx(&self, artifact: JitCompileArtifact, ctx: *mut u8) -> i64 {
        // SAFETY: function has signature (*mut u8) -> i64
        let func: extern "C" fn(*mut u8) -> i64 =
            unsafe { std::mem::transmute(artifact.code_ptr) };
        func(ctx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime_helpers::HelperKind;
    use otter_vm_bytecode::{
        ConstantIndex, FunctionIndex, Instruction, JumpOffset, LocalIndex, Register,
    };

    const TAG_UNDEFINED: i64 = 0x7FF8_0000_0000_0000_u64 as i64;
    const TAG_NULL: i64 = 0x7FF8_0000_0000_0001_u64 as i64;
    const TAG_TRUE: i64 = 0x7FF8_0000_0000_0002_u64 as i64;
    const TAG_FALSE: i64 = 0x7FF8_0000_0000_0003_u64 as i64;
    const TAG_INT32: i64 = 0x7FF8_0001_0000_0000_u64 as i64;
    const TAG_NAN: i64 = 0x7FFA_0000_0000_0000_u64 as i64;

    /// NaN-box an i32 constant (matching interpreter representation).
    fn nb(n: i32) -> i64 {
        TAG_INT32 | ((n as u32) as i64)
    }

    // ===== Pure computation tests (no helpers — int32 fast paths only) =====

    #[test]
    fn basic_compile() {
        let function = Function::builder()
            .name("basic_compile")
            .register_count(1)
            .instruction(Instruction::LoadInt32 {
                dst: Register(0),
                value: 7,
            })
            .instruction(Instruction::Return { src: Register(0) })
            .build();
        let mut jit = JitCompiler::new().expect("jit init");
        let artifact = jit.compile_function(&function).expect("compile");
        assert!(!artifact.code_ptr.is_null());
        assert_eq!(jit.execute_compiled_i64(artifact), nb(7));
    }

    #[test]
    fn translation_arithmetic_returns_value() {
        let function = Function::builder()
            .name("arith")
            .register_count(4)
            .instruction(Instruction::LoadInt32 {
                dst: Register(0),
                value: 2,
            })
            .instruction(Instruction::LoadInt32 {
                dst: Register(1),
                value: 3,
            })
            .instruction(Instruction::Add {
                dst: Register(2),
                lhs: Register(0),
                rhs: Register(1),
                feedback_index: 0,
            })
            .instruction(Instruction::Return { src: Register(2) })
            .feedback_vector_size(1)
            .build();
        let mut jit = JitCompiler::new().expect("jit init");
        let artifact = jit.compile_function(&function).expect("compile");
        assert_eq!(jit.execute_compiled_i64(artifact), nb(5));
    }

    #[test]
    fn translation_sub_returns_value() {
        let f = Function::builder()
            .name("sub")
            .register_count(3)
            .instruction(Instruction::LoadInt32 {
                dst: Register(0),
                value: 10,
            })
            .instruction(Instruction::LoadInt32 {
                dst: Register(1),
                value: 3,
            })
            .instruction(Instruction::Sub {
                dst: Register(2),
                lhs: Register(0),
                rhs: Register(1),
                feedback_index: 0,
            })
            .instruction(Instruction::Return { src: Register(2) })
            .feedback_vector_size(1)
            .build();
        let mut jit = JitCompiler::new().expect("jit init");
        let a = jit.compile_function(&f).expect("compile");
        assert_eq!(jit.execute_compiled_i64(a), nb(7));
    }

    #[test]
    fn translation_mul_returns_value() {
        let f = Function::builder()
            .name("mul")
            .register_count(3)
            .instruction(Instruction::LoadInt32 {
                dst: Register(0),
                value: 6,
            })
            .instruction(Instruction::LoadInt32 {
                dst: Register(1),
                value: 7,
            })
            .instruction(Instruction::Mul {
                dst: Register(2),
                lhs: Register(0),
                rhs: Register(1),
                feedback_index: 0,
            })
            .instruction(Instruction::Return { src: Register(2) })
            .feedback_vector_size(1)
            .build();
        let mut jit = JitCompiler::new().expect("jit init");
        let a = jit.compile_function(&f).expect("compile");
        assert_eq!(jit.execute_compiled_i64(a), nb(42));
    }

    #[test]
    fn translation_div_exact() {
        // 10 / 2 = 5 (exact division → i32 fast path)
        let f = Function::builder()
            .name("div")
            .register_count(3)
            .instruction(Instruction::LoadInt32 {
                dst: Register(0),
                value: 10,
            })
            .instruction(Instruction::LoadInt32 {
                dst: Register(1),
                value: 2,
            })
            .instruction(Instruction::Div {
                dst: Register(2),
                lhs: Register(0),
                rhs: Register(1),
                feedback_index: 0,
            })
            .instruction(Instruction::Return { src: Register(2) })
            .feedback_vector_size(1)
            .build();
        let mut jit = JitCompiler::new().expect("jit init");
        let a = jit.compile_function(&f).expect("compile");
        assert_eq!(jit.execute_compiled_i64(a), nb(5));
    }

    #[test]
    fn translation_negative_arithmetic() {
        // -5 + 3 = -2 (negative i32 fast path)
        let f = Function::builder()
            .name("neg_arith")
            .register_count(3)
            .instruction(Instruction::LoadInt32 {
                dst: Register(0),
                value: -5,
            })
            .instruction(Instruction::LoadInt32 {
                dst: Register(1),
                value: 3,
            })
            .instruction(Instruction::Add {
                dst: Register(2),
                lhs: Register(0),
                rhs: Register(1),
                feedback_index: 0,
            })
            .instruction(Instruction::Return { src: Register(2) })
            .feedback_vector_size(1)
            .build();
        let mut jit = JitCompiler::new().expect("jit init");
        let a = jit.compile_function(&f).expect("compile");
        assert_eq!(jit.execute_compiled_i64(a), nb(-2));
    }

    #[test]
    fn translation_control_flow_jump_if_true() {
        // LoadInt32(1) is NaN-boxed int32(1) which is truthy
        let function = Function::builder()
            .name("cf")
            .register_count(3)
            .instruction(Instruction::LoadTrue { dst: Register(0) })
            .instruction(Instruction::JumpIfTrue {
                cond: Register(0),
                offset: JumpOffset(3),
            })
            .instruction(Instruction::LoadInt32 {
                dst: Register(1),
                value: 7,
            })
            .instruction(Instruction::Return { src: Register(1) })
            .instruction(Instruction::LoadInt32 {
                dst: Register(2),
                value: 42,
            })
            .instruction(Instruction::Return { src: Register(2) })
            .build();
        let mut jit = JitCompiler::new().expect("jit init");
        let artifact = jit.compile_function(&function).expect("compile");
        assert_eq!(jit.execute_compiled_i64(artifact), nb(42));
    }

    #[test]
    fn translation_jump_if_true_with_int32() {
        // int32(1) should be truthy, int32(0) should be falsy
        let f = Function::builder()
            .name("jit_i32")
            .register_count(3)
            .instruction(Instruction::LoadInt32 {
                dst: Register(0),
                value: 1,
            })
            .instruction(Instruction::JumpIfTrue {
                cond: Register(0),
                offset: JumpOffset(3),
            })
            .instruction(Instruction::LoadInt32 {
                dst: Register(1),
                value: 7,
            })
            .instruction(Instruction::Return { src: Register(1) })
            .instruction(Instruction::LoadInt32 {
                dst: Register(2),
                value: 42,
            })
            .instruction(Instruction::Return { src: Register(2) })
            .build();
        let mut jit = JitCompiler::new().expect("jit init");
        let a = jit.compile_function(&f).expect("compile");
        assert_eq!(jit.execute_compiled_i64(a), nb(42));
    }

    #[test]
    fn translation_reports_unsupported_instruction() {
        let function = Function::builder()
            .name("unsupported")
            .register_count(1)
            .instruction(Instruction::Yield {
                dst: Register(0),
                src: Register(0),
            })
            .instruction(Instruction::Return { src: Register(0) })
            .build();
        let mut jit = JitCompiler::new().expect("jit init");
        let err = jit
            .compile_function(&function)
            .expect_err("should fail");
        assert!(matches!(
            err,
            JitError::UnsupportedInstruction { pc: 0, .. }
        ));
    }

    #[test]
    fn translation_load_constants() {
        let mut jit = JitCompiler::new().expect("jit init");
        for (name, instr, expected) in [
            (
                "undef",
                Instruction::LoadUndefined { dst: Register(0) },
                TAG_UNDEFINED,
            ),
            (
                "null",
                Instruction::LoadNull { dst: Register(0) },
                TAG_NULL,
            ),
            (
                "true",
                Instruction::LoadTrue { dst: Register(0) },
                TAG_TRUE,
            ),
            (
                "false",
                Instruction::LoadFalse { dst: Register(0) },
                TAG_FALSE,
            ),
        ] {
            let f = Function::builder()
                .name(name)
                .register_count(1)
                .instruction(instr)
                .instruction(Instruction::Return { src: Register(0) })
                .build();
            let a = jit.compile_function(&f).expect("compile");
            assert_eq!(jit.execute_compiled_i64(a), expected, "failed for {name}");
        }
    }

    #[test]
    fn translation_return_undefined_returns_tag() {
        let f = Function::builder()
            .name("ru")
            .register_count(0)
            .instruction(Instruction::ReturnUndefined)
            .build();
        let mut jit = JitCompiler::new().expect("jit init");
        let a = jit.compile_function(&f).expect("compile");
        assert_eq!(jit.execute_compiled_i64(a), TAG_UNDEFINED);
    }

    #[test]
    fn translation_bitwise_operations() {
        let f = Function::builder()
            .name("bw")
            .register_count(3)
            .instruction(Instruction::LoadInt32 {
                dst: Register(0),
                value: 0xFF,
            })
            .instruction(Instruction::LoadInt32 {
                dst: Register(1),
                value: 0x0F,
            })
            .instruction(Instruction::BitAnd {
                dst: Register(2),
                lhs: Register(0),
                rhs: Register(1),
            })
            .instruction(Instruction::Return { src: Register(2) })
            .build();
        let mut jit = JitCompiler::new().expect("jit init");
        let a = jit.compile_function(&f).expect("compile");
        assert_eq!(jit.execute_compiled_i64(a), nb(0x0F));
    }

    #[test]
    fn translation_comparison_lt() {
        let f = Function::builder()
            .name("cmp")
            .register_count(3)
            .instruction(Instruction::LoadInt32 {
                dst: Register(0),
                value: 5,
            })
            .instruction(Instruction::LoadInt32 {
                dst: Register(1),
                value: 10,
            })
            .instruction(Instruction::Lt {
                dst: Register(2),
                lhs: Register(0),
                rhs: Register(1),
            })
            .instruction(Instruction::Return { src: Register(2) })
            .build();
        let mut jit = JitCompiler::new().expect("jit init");
        let a = jit.compile_function(&f).expect("compile");
        assert_eq!(jit.execute_compiled_i64(a), TAG_TRUE);
    }

    #[test]
    fn translation_comparison_negative() {
        // -1 < 1 should be true (signed comparison on unboxed i32)
        let f = Function::builder()
            .name("cmp_neg")
            .register_count(3)
            .instruction(Instruction::LoadInt32 {
                dst: Register(0),
                value: -1,
            })
            .instruction(Instruction::LoadInt32 {
                dst: Register(1),
                value: 1,
            })
            .instruction(Instruction::Lt {
                dst: Register(2),
                lhs: Register(0),
                rhs: Register(1),
            })
            .instruction(Instruction::Return { src: Register(2) })
            .build();
        let mut jit = JitCompiler::new().expect("jit init");
        let a = jit.compile_function(&f).expect("compile");
        assert_eq!(jit.execute_compiled_i64(a), TAG_TRUE);
    }

    #[test]
    fn translation_strict_eq() {
        let f = Function::builder()
            .name("seq")
            .register_count(3)
            .instruction(Instruction::LoadInt32 {
                dst: Register(0),
                value: 42,
            })
            .instruction(Instruction::LoadInt32 {
                dst: Register(1),
                value: 42,
            })
            .instruction(Instruction::StrictEq {
                dst: Register(2),
                lhs: Register(0),
                rhs: Register(1),
            })
            .instruction(Instruction::Return { src: Register(2) })
            .build();
        let mut jit = JitCompiler::new().expect("jit init");
        let a = jit.compile_function(&f).expect("compile");
        assert_eq!(jit.execute_compiled_i64(a), TAG_TRUE);
    }

    #[test]
    fn translation_strict_ne() {
        let f = Function::builder()
            .name("sne")
            .register_count(3)
            .instruction(Instruction::LoadInt32 {
                dst: Register(0),
                value: 1,
            })
            .instruction(Instruction::LoadInt32 {
                dst: Register(1),
                value: 2,
            })
            .instruction(Instruction::StrictNe {
                dst: Register(2),
                lhs: Register(0),
                rhs: Register(1),
            })
            .instruction(Instruction::Return { src: Register(2) })
            .build();
        let mut jit = JitCompiler::new().expect("jit init");
        let a = jit.compile_function(&f).expect("compile");
        assert_eq!(jit.execute_compiled_i64(a), TAG_TRUE);
    }

    #[test]
    fn strict_eq_nan_is_not_equal() {
        // NaN === NaN must be false (TAG_NAN bits are the same, but JS says not equal)
        let constants: Vec<i64> = vec![TAG_NAN];
        let ctx_data = (constants.as_ptr(), constants.len());
        let f = Function::builder()
            .name("nan_eq")
            .register_count(3)
            .instruction(Instruction::LoadConst {
                dst: Register(0),
                idx: ConstantIndex(0),
            })
            .instruction(Instruction::LoadConst {
                dst: Register(1),
                idx: ConstantIndex(0),
            })
            .instruction(Instruction::StrictEq {
                dst: Register(2),
                lhs: Register(0),
                rhs: Register(1),
            })
            .instruction(Instruction::Return { src: Register(2) })
            .build();
        let mut jit = JitCompiler::with_helpers(make_all_helpers()).expect("jit init");
        let a = jit.compile_function(&f).expect("compile");
        assert_eq!(
            jit.execute_with_ctx(a, &ctx_data as *const _ as *mut u8),
            TAG_FALSE, // NaN === NaN → false
        );
    }

    #[test]
    fn strict_ne_nan_is_not_equal() {
        // NaN !== NaN must be true
        let constants: Vec<i64> = vec![TAG_NAN];
        let ctx_data = (constants.as_ptr(), constants.len());
        let f = Function::builder()
            .name("nan_ne")
            .register_count(3)
            .instruction(Instruction::LoadConst {
                dst: Register(0),
                idx: ConstantIndex(0),
            })
            .instruction(Instruction::LoadConst {
                dst: Register(1),
                idx: ConstantIndex(0),
            })
            .instruction(Instruction::StrictNe {
                dst: Register(2),
                lhs: Register(0),
                rhs: Register(1),
            })
            .instruction(Instruction::Return { src: Register(2) })
            .build();
        let mut jit = JitCompiler::with_helpers(make_all_helpers()).expect("jit init");
        let a = jit.compile_function(&f).expect("compile");
        assert_eq!(
            jit.execute_with_ctx(a, &ctx_data as *const _ as *mut u8),
            TAG_TRUE, // NaN !== NaN → true
        );
    }

    #[test]
    fn strict_eq_pos_neg_zero() {
        // +0.0 === -0.0 must be true (different bits, but JS says equal)
        let pos_zero = 0_i64; // f64 +0.0 bit pattern
        let neg_zero = 0x8000_0000_0000_0000_u64 as i64; // f64 -0.0 bit pattern
        let constants: Vec<i64> = vec![pos_zero, neg_zero];
        let ctx_data = (constants.as_ptr(), constants.len());
        let f = Function::builder()
            .name("zero_eq")
            .register_count(3)
            .instruction(Instruction::LoadConst {
                dst: Register(0),
                idx: ConstantIndex(0),
            })
            .instruction(Instruction::LoadConst {
                dst: Register(1),
                idx: ConstantIndex(1),
            })
            .instruction(Instruction::StrictEq {
                dst: Register(2),
                lhs: Register(0),
                rhs: Register(1),
            })
            .instruction(Instruction::Return { src: Register(2) })
            .build();
        let mut jit = JitCompiler::with_helpers(make_all_helpers()).expect("jit init");
        let a = jit.compile_function(&f).expect("compile");
        assert_eq!(
            jit.execute_with_ctx(a, &ctx_data as *const _ as *mut u8),
            TAG_TRUE, // +0 === -0 → true
        );
    }

    #[test]
    fn strict_ne_pos_neg_zero() {
        // +0.0 !== -0.0 must be false
        let pos_zero = 0_i64;
        let neg_zero = 0x8000_0000_0000_0000_u64 as i64;
        let constants: Vec<i64> = vec![pos_zero, neg_zero];
        let ctx_data = (constants.as_ptr(), constants.len());
        let f = Function::builder()
            .name("zero_ne")
            .register_count(3)
            .instruction(Instruction::LoadConst {
                dst: Register(0),
                idx: ConstantIndex(0),
            })
            .instruction(Instruction::LoadConst {
                dst: Register(1),
                idx: ConstantIndex(1),
            })
            .instruction(Instruction::StrictNe {
                dst: Register(2),
                lhs: Register(0),
                rhs: Register(1),
            })
            .instruction(Instruction::Return { src: Register(2) })
            .build();
        let mut jit = JitCompiler::with_helpers(make_all_helpers()).expect("jit init");
        let a = jit.compile_function(&f).expect("compile");
        assert_eq!(
            jit.execute_with_ctx(a, &ctx_data as *const _ as *mut u8),
            TAG_FALSE, // +0 !== -0 → false
        );
    }

    #[test]
    fn translation_inc_dec() {
        let f = Function::builder()
            .name("inc")
            .register_count(2)
            .instruction(Instruction::LoadInt32 {
                dst: Register(0),
                value: 10,
            })
            .instruction(Instruction::Inc {
                dst: Register(1),
                src: Register(0),
            })
            .instruction(Instruction::Return { src: Register(1) })
            .build();
        let mut jit = JitCompiler::new().expect("jit init");
        let a = jit.compile_function(&f).expect("compile");
        assert_eq!(jit.execute_compiled_i64(a), nb(11));
    }

    #[test]
    fn translation_dec() {
        let f = Function::builder()
            .name("dec")
            .register_count(2)
            .instruction(Instruction::LoadInt32 {
                dst: Register(0),
                value: 10,
            })
            .instruction(Instruction::Dec {
                dst: Register(1),
                src: Register(0),
            })
            .instruction(Instruction::Return { src: Register(1) })
            .build();
        let mut jit = JitCompiler::new().expect("jit init");
        let a = jit.compile_function(&f).expect("compile");
        assert_eq!(jit.execute_compiled_i64(a), nb(9));
    }

    #[test]
    fn translation_neg() {
        let f = Function::builder()
            .name("neg")
            .register_count(2)
            .instruction(Instruction::LoadInt32 {
                dst: Register(0),
                value: 5,
            })
            .instruction(Instruction::Neg {
                dst: Register(1),
                src: Register(0),
            })
            .instruction(Instruction::Return { src: Register(1) })
            .build();
        let mut jit = JitCompiler::new().expect("jit init");
        let a = jit.compile_function(&f).expect("compile");
        assert_eq!(jit.execute_compiled_i64(a), nb(-5));
    }

    #[test]
    fn translation_local_variables() {
        let f = Function::builder()
            .name("lv")
            .register_count(2)
            .local_count(1)
            .instruction(Instruction::LoadInt32 {
                dst: Register(0),
                value: 42,
            })
            .instruction(Instruction::SetLocal {
                idx: LocalIndex(0),
                src: Register(0),
            })
            .instruction(Instruction::GetLocal {
                dst: Register(1),
                idx: LocalIndex(0),
            })
            .instruction(Instruction::Return { src: Register(1) })
            .build();
        let mut jit = JitCompiler::new().expect("jit init");
        let a = jit.compile_function(&f).expect("compile");
        assert_eq!(jit.execute_compiled_i64(a), nb(42));
    }

    #[test]
    fn translation_logical_not_of_falsy() {
        // !int32(0) → TAG_TRUE (0 is falsy)
        let f = Function::builder()
            .name("not0")
            .register_count(2)
            .instruction(Instruction::LoadInt32 {
                dst: Register(0),
                value: 0,
            })
            .instruction(Instruction::Not {
                dst: Register(1),
                src: Register(0),
            })
            .instruction(Instruction::Return { src: Register(1) })
            .build();
        let mut jit = JitCompiler::new().expect("jit init");
        let a = jit.compile_function(&f).expect("compile");
        assert_eq!(jit.execute_compiled_i64(a), TAG_TRUE);
    }

    #[test]
    fn translation_logical_not_of_truthy() {
        // !int32(5) → TAG_FALSE (5 is truthy)
        let f = Function::builder()
            .name("not5")
            .register_count(2)
            .instruction(Instruction::LoadInt32 {
                dst: Register(0),
                value: 5,
            })
            .instruction(Instruction::Not {
                dst: Register(1),
                src: Register(0),
            })
            .instruction(Instruction::Return { src: Register(1) })
            .build();
        let mut jit = JitCompiler::new().expect("jit init");
        let a = jit.compile_function(&f).expect("compile");
        assert_eq!(jit.execute_compiled_i64(a), TAG_FALSE);
    }

    #[test]
    fn translation_logical_not_of_false() {
        // !false → TAG_TRUE
        let f = Function::builder()
            .name("not_false")
            .register_count(2)
            .instruction(Instruction::LoadFalse { dst: Register(0) })
            .instruction(Instruction::Not {
                dst: Register(1),
                src: Register(0),
            })
            .instruction(Instruction::Return { src: Register(1) })
            .build();
        let mut jit = JitCompiler::new().expect("jit init");
        let a = jit.compile_function(&f).expect("compile");
        assert_eq!(jit.execute_compiled_i64(a), TAG_TRUE);
    }

    #[test]
    fn translation_mod_operation() {
        let f = Function::builder()
            .name("mod")
            .register_count(3)
            .instruction(Instruction::LoadInt32 {
                dst: Register(0),
                value: 17,
            })
            .instruction(Instruction::LoadInt32 {
                dst: Register(1),
                value: 5,
            })
            .instruction(Instruction::Mod {
                dst: Register(2),
                lhs: Register(0),
                rhs: Register(1),
            })
            .instruction(Instruction::Return { src: Register(2) })
            .build();
        let mut jit = JitCompiler::new().expect("jit init");
        let a = jit.compile_function(&f).expect("compile");
        assert_eq!(jit.execute_compiled_i64(a), nb(2));
    }

    #[test]
    fn translation_jump_if_false() {
        // int32(0) is falsy → should jump
        let f = Function::builder()
            .name("jif")
            .register_count(2)
            .instruction(Instruction::LoadInt32 {
                dst: Register(0),
                value: 0,
            })
            .instruction(Instruction::JumpIfFalse {
                cond: Register(0),
                offset: JumpOffset(3),
            })
            .instruction(Instruction::LoadInt32 {
                dst: Register(1),
                value: 7,
            })
            .instruction(Instruction::Return { src: Register(1) })
            .instruction(Instruction::LoadInt32 {
                dst: Register(1),
                value: 99,
            })
            .instruction(Instruction::Return { src: Register(1) })
            .build();
        let mut jit = JitCompiler::new().expect("jit init");
        let a = jit.compile_function(&f).expect("compile");
        assert_eq!(jit.execute_compiled_i64(a), nb(99));
    }

    #[test]
    fn translation_nop_and_debugger() {
        let f = Function::builder()
            .name("nop")
            .register_count(1)
            .instruction(Instruction::Nop)
            .instruction(Instruction::Debugger)
            .instruction(Instruction::LoadInt32 {
                dst: Register(0),
                value: 77,
            })
            .instruction(Instruction::Return { src: Register(0) })
            .build();
        let mut jit = JitCompiler::new().expect("jit init");
        let a = jit.compile_function(&f).expect("compile");
        assert_eq!(jit.execute_compiled_i64(a), nb(77));
    }

    // ===== Type guard specific tests =====

    #[test]
    fn type_guards_i32_add_no_overflow() {
        // 100 + 200 = 300 — well within i32 range
        let f = Function::builder()
            .name("tg_add")
            .register_count(3)
            .instruction(Instruction::LoadInt32 {
                dst: Register(0),
                value: 100,
            })
            .instruction(Instruction::LoadInt32 {
                dst: Register(1),
                value: 200,
            })
            .instruction(Instruction::Add {
                dst: Register(2),
                lhs: Register(0),
                rhs: Register(1),
                feedback_index: 0,
            })
            .instruction(Instruction::Return { src: Register(2) })
            .feedback_vector_size(1)
            .build();
        let mut jit = JitCompiler::new().expect("jit init");
        let a = jit.compile_function(&f).expect("compile");
        assert_eq!(jit.execute_compiled_i64(a), nb(300));
    }

    #[test]
    fn type_guards_comparison_chain() {
        // Test: (5 < 10) is true, then JumpIfTrue should take the branch
        let f = Function::builder()
            .name("tg_cmp_chain")
            .register_count(4)
            .instruction(Instruction::LoadInt32 {
                dst: Register(0),
                value: 5,
            })
            .instruction(Instruction::LoadInt32 {
                dst: Register(1),
                value: 10,
            })
            .instruction(Instruction::Lt {
                dst: Register(2),
                lhs: Register(0),
                rhs: Register(1),
            })
            .instruction(Instruction::JumpIfTrue {
                cond: Register(2),
                offset: JumpOffset(3),
            })
            // pc4: not taken path
            .instruction(Instruction::LoadInt32 {
                dst: Register(3),
                value: 0,
            })
            .instruction(Instruction::Return { src: Register(3) })
            // pc6: jumped here (3 + 3 = 6)
            .instruction(Instruction::LoadInt32 {
                dst: Register(3),
                value: 1,
            })
            .instruction(Instruction::Return { src: Register(3) })
            .build();
        let mut jit = JitCompiler::new().expect("jit init");
        let a = jit.compile_function(&f).expect("compile");
        assert_eq!(jit.execute_compiled_i64(a), nb(1));
    }

    #[test]
    fn type_guards_bitwise_or_and_xor() {
        // 0xF0 | 0x0F = 0xFF
        let f = Function::builder()
            .name("tg_bor")
            .register_count(3)
            .instruction(Instruction::LoadInt32 {
                dst: Register(0),
                value: 0xF0,
            })
            .instruction(Instruction::LoadInt32 {
                dst: Register(1),
                value: 0x0F,
            })
            .instruction(Instruction::BitOr {
                dst: Register(2),
                lhs: Register(0),
                rhs: Register(1),
            })
            .instruction(Instruction::Return { src: Register(2) })
            .build();
        let mut jit = JitCompiler::new().expect("jit init");
        let a = jit.compile_function(&f).expect("compile");
        assert_eq!(jit.execute_compiled_i64(a), nb(0xFF));
    }

    #[test]
    fn type_guards_bit_not() {
        // ~0 = -1 (all bits set)
        let f = Function::builder()
            .name("tg_bnot")
            .register_count(2)
            .instruction(Instruction::LoadInt32 {
                dst: Register(0),
                value: 0,
            })
            .instruction(Instruction::BitNot {
                dst: Register(1),
                src: Register(0),
            })
            .instruction(Instruction::Return { src: Register(1) })
            .build();
        let mut jit = JitCompiler::new().expect("jit init");
        let a = jit.compile_function(&f).expect("compile");
        assert_eq!(jit.execute_compiled_i64(a), nb(-1));
    }

    #[test]
    fn type_guards_shift_left() {
        // 1 << 4 = 16
        let f = Function::builder()
            .name("tg_shl")
            .register_count(3)
            .instruction(Instruction::LoadInt32 {
                dst: Register(0),
                value: 1,
            })
            .instruction(Instruction::LoadInt32 {
                dst: Register(1),
                value: 4,
            })
            .instruction(Instruction::Shl {
                dst: Register(2),
                lhs: Register(0),
                rhs: Register(1),
            })
            .instruction(Instruction::Return { src: Register(2) })
            .build();
        let mut jit = JitCompiler::new().expect("jit init");
        let a = jit.compile_function(&f).expect("compile");
        assert_eq!(jit.execute_compiled_i64(a), nb(16));
    }

    #[test]
    fn type_guards_nullish_check() {
        // JumpIfNullish with null → should jump
        let f = Function::builder()
            .name("tg_nullish")
            .register_count(2)
            .instruction(Instruction::LoadNull { dst: Register(0) })
            .instruction(Instruction::JumpIfNullish {
                src: Register(0),
                offset: JumpOffset(3),
            })
            .instruction(Instruction::LoadInt32 {
                dst: Register(1),
                value: 0,
            })
            .instruction(Instruction::Return { src: Register(1) })
            .instruction(Instruction::LoadInt32 {
                dst: Register(1),
                value: 1,
            })
            .instruction(Instruction::Return { src: Register(1) })
            .build();
        let mut jit = JitCompiler::new().expect("jit init");
        let a = jit.compile_function(&f).expect("compile");
        assert_eq!(jit.execute_compiled_i64(a), nb(1));
    }

    // ===== Feedback-driven specialization tests =====

    /// Helper to set type observations on a function's feedback vector entry.
    fn set_feedback_f64_only(function: &Function, index: usize) {
        let fv = function.feedback_vector.write();
        if let Some(entry) = fv.get_mut(index) {
            entry.type_observations.observe_number();
        }
    }

    fn set_feedback_int32_only(function: &Function, index: usize) {
        let fv = function.feedback_vector.write();
        if let Some(entry) = fv.get_mut(index) {
            entry.type_observations.observe_int32();
        }
    }

    fn set_feedback_numeric(function: &Function, index: usize) {
        let fv = function.feedback_vector.write();
        if let Some(entry) = fv.get_mut(index) {
            entry.type_observations.observe_int32();
            entry.type_observations.observe_number();
        }
    }

    fn set_feedback_string(function: &Function, index: usize) {
        let fv = function.feedback_vector.write();
        if let Some(entry) = fv.get_mut(index) {
            entry.type_observations.observe_string();
        }
    }

    #[test]
    fn feedback_int32_only_uses_i32_guard() {
        // With int32-only feedback, pure int32 arithmetic should work (same as default)
        let f = Function::builder()
            .name("fb_i32")
            .register_count(3)
            .instruction(Instruction::LoadInt32 {
                dst: Register(0),
                value: 7,
            })
            .instruction(Instruction::LoadInt32 {
                dst: Register(1),
                value: 3,
            })
            .instruction(Instruction::Add {
                dst: Register(2),
                lhs: Register(0),
                rhs: Register(1),
                feedback_index: 0,
            })
            .instruction(Instruction::Return { src: Register(2) })
            .feedback_vector_size(1)
            .build();
        set_feedback_int32_only(&f, 0);
        let mut jit = JitCompiler::new().expect("jit init");
        let a = jit.compile_function(&f).expect("compile");
        assert_eq!(jit.execute_compiled_i64(a), nb(10));
    }

    #[test]
    fn feedback_f64_specialization_add() {
        // With f64-only feedback, should use f64 fast path.
        // Since we're loading int32 constants (not f64), the f64 guard will FAIL
        // and return BAILOUT_SENTINEL (no generic helper available).
        // This test verifies compilation succeeds.
        let f = Function::builder()
            .name("fb_f64_compile")
            .register_count(3)
            .instruction(Instruction::LoadInt32 {
                dst: Register(0),
                value: 7,
            })
            .instruction(Instruction::LoadInt32 {
                dst: Register(1),
                value: 3,
            })
            .instruction(Instruction::Add {
                dst: Register(2),
                lhs: Register(0),
                rhs: Register(1),
                feedback_index: 0,
            })
            .instruction(Instruction::Return { src: Register(2) })
            .feedback_vector_size(1)
            .build();
        set_feedback_f64_only(&f, 0);
        // Just verify compilation succeeds — f64 guard + trap slow path
        let mut jit = JitCompiler::new().expect("jit init");
        let _a = jit.compile_function(&f).expect("compile should succeed with f64 hint");
    }

    #[test]
    fn feedback_numeric_cascading_guard() {
        // With numeric feedback (both int32 and f64 seen), should use cascading guard.
        // int32 values will hit the i32 fast path.
        let f = Function::builder()
            .name("fb_numeric")
            .register_count(3)
            .instruction(Instruction::LoadInt32 {
                dst: Register(0),
                value: 15,
            })
            .instruction(Instruction::LoadInt32 {
                dst: Register(1),
                value: 5,
            })
            .instruction(Instruction::Sub {
                dst: Register(2),
                lhs: Register(0),
                rhs: Register(1),
                feedback_index: 0,
            })
            .instruction(Instruction::Return { src: Register(2) })
            .feedback_vector_size(1)
            .build();
        set_feedback_numeric(&f, 0);
        let mut jit = JitCompiler::new().expect("jit init");
        let a = jit.compile_function(&f).expect("compile");
        // int32 values still hit the i32 fast path in the cascading guard
        assert_eq!(jit.execute_compiled_i64(a), nb(10));
    }

    #[test]
    fn feedback_string_uses_generic() {
        // With string feedback, should go directly to generic helper.
        // Without helpers registered, compilation should fail for Generic specialization.
        let f = Function::builder()
            .name("fb_string")
            .register_count(3)
            .instruction(Instruction::LoadInt32 {
                dst: Register(0),
                value: 1,
            })
            .instruction(Instruction::LoadInt32 {
                dst: Register(1),
                value: 2,
            })
            .instruction(Instruction::Add {
                dst: Register(2),
                lhs: Register(0),
                rhs: Register(1),
                feedback_index: 0,
            })
            .instruction(Instruction::Return { src: Register(2) })
            .feedback_vector_size(1)
            .build();
        set_feedback_string(&f, 0);
        let mut jit = JitCompiler::new().expect("jit init");
        // Generic specialization requires helper — should fail without one
        let err = jit.compile_function(&f).expect_err("should fail without generic helper");
        assert!(matches!(err, JitError::UnsupportedInstruction { pc: 2, .. }));
    }

    extern "C" fn mock_generic_add(_ctx: *mut u8, lhs: i64, rhs: i64) -> i64 {
        // Simple mock: just return lhs (for testing that the generic path is taken)
        let _ = rhs;
        lhs
    }

    #[test]
    fn feedback_string_with_helper_succeeds() {
        // With string feedback + GenericAdd helper registered, should compile and run
        let f = Function::builder()
            .name("fb_string_ok")
            .register_count(3)
            .instruction(Instruction::LoadInt32 {
                dst: Register(0),
                value: 1,
            })
            .instruction(Instruction::LoadInt32 {
                dst: Register(1),
                value: 2,
            })
            .instruction(Instruction::Add {
                dst: Register(2),
                lhs: Register(0),
                rhs: Register(1),
                feedback_index: 0,
            })
            .instruction(Instruction::Return { src: Register(2) })
            .feedback_vector_size(1)
            .build();
        set_feedback_string(&f, 0);
        let mut h = RuntimeHelpers::new();
        unsafe { h.set(HelperKind::GenericAdd, mock_generic_add as *const u8); }
        let mut jit = JitCompiler::with_helpers(h).expect("jit init");
        let a = jit.compile_function(&f).expect("compile");
        // Generic path calls mock_generic_add which returns lhs (NaN-boxed 1)
        assert_eq!(jit.execute_compiled_i64(a), nb(1));
    }

    #[test]
    fn feedback_f64_div_specialization() {
        // Division with f64-only feedback should compile with f64 guard
        let f = Function::builder()
            .name("fb_f64_div")
            .register_count(3)
            .instruction(Instruction::LoadInt32 {
                dst: Register(0),
                value: 10,
            })
            .instruction(Instruction::LoadInt32 {
                dst: Register(1),
                value: 3,
            })
            .instruction(Instruction::Div {
                dst: Register(2),
                lhs: Register(0),
                rhs: Register(1),
                feedback_index: 0,
            })
            .instruction(Instruction::Return { src: Register(2) })
            .feedback_vector_size(1)
            .build();
        set_feedback_f64_only(&f, 0);
        let mut jit = JitCompiler::new().expect("jit init");
        // Compilation should succeed
        let _a = jit.compile_function(&f).expect("compile with f64 div hint");
    }

    #[test]
    fn feedback_numeric_mul_cascading() {
        // Multiplication with numeric feedback — cascading i32 → f64 → generic
        let f = Function::builder()
            .name("fb_num_mul")
            .register_count(3)
            .instruction(Instruction::LoadInt32 {
                dst: Register(0),
                value: 6,
            })
            .instruction(Instruction::LoadInt32 {
                dst: Register(1),
                value: 7,
            })
            .instruction(Instruction::Mul {
                dst: Register(2),
                lhs: Register(0),
                rhs: Register(1),
                feedback_index: 0,
            })
            .instruction(Instruction::Return { src: Register(2) })
            .feedback_vector_size(1)
            .build();
        set_feedback_numeric(&f, 0);
        let mut jit = JitCompiler::new().expect("jit init");
        let a = jit.compile_function(&f).expect("compile");
        assert_eq!(jit.execute_compiled_i64(a), nb(42));
    }

    // ===== Runtime helper tests =====

    extern "C" fn mock_load_const(ctx: *mut u8, idx: i64) -> i64 {
        if ctx.is_null() {
            return TAG_UNDEFINED;
        }
        unsafe {
            let (ptr, len) = *(ctx as *const (*const i64, usize));
            if (idx as usize) < len {
                *ptr.add(idx as usize)
            } else {
                TAG_UNDEFINED
            }
        }
    }
    extern "C" fn mock_get_global(_ctx: *mut u8, name_idx: i64, _ic: i64) -> i64 {
        name_idx * 100
    }
    extern "C" fn mock_new_object(_ctx: *mut u8) -> i64 {
        0xDEAD_BEEF_i64
    }
    extern "C" fn mock_new_array(_ctx: *mut u8, len: i64) -> i64 {
        0xBEEF_0000_i64 + len
    }
    extern "C" fn mock_get_prop_const(_ctx: *mut u8, obj: i64, name: i64, _ic: i64) -> i64 {
        obj + name
    }
    extern "C" fn mock_set_prop_const(
        _ctx: *mut u8,
        _obj: i64,
        _name: i64,
        val: i64,
        _ic: i64,
    ) -> i64 {
        val
    }
    extern "C" fn mock_get_prop(_ctx: *mut u8, obj: i64, key: i64, _ic: i64) -> i64 {
        obj + key
    }
    extern "C" fn mock_set_prop(
        _ctx: *mut u8,
        _obj: i64,
        _key: i64,
        val: i64,
        _ic: i64,
    ) -> i64 {
        val
    }
    extern "C" fn mock_call_function(
        _ctx: *mut u8,
        _callee: i64,
        argc: i64,
        argv: *const i64,
    ) -> i64 {
        // Return first arg if available, otherwise argc
        if argc > 0 && !argv.is_null() {
            unsafe { *argv }
        } else {
            argc
        }
    }
    extern "C" fn mock_create_closure(_ctx: *mut u8, func_idx: i64) -> i64 {
        func_idx + 1000
    }
    extern "C" fn mock_throw(_ctx: *mut u8, val: i64) -> i64 {
        val
    }
    extern "C" fn mock_get_elem(_ctx: *mut u8, obj: i64, idx: i64, _ic: i64) -> i64 {
        obj + idx * 10
    }
    extern "C" fn mock_set_elem(
        _ctx: *mut u8,
        _obj: i64,
        _idx: i64,
        val: i64,
        _ic: i64,
    ) -> i64 {
        val
    }
    extern "C" fn mock_define_prop(_ctx: *mut u8, _obj: i64, _key: i64, val: i64) -> i64 {
        val
    }
    extern "C" fn mock_delete_prop(_ctx: *mut u8, _obj: i64, _key: i64) -> i64 {
        TAG_TRUE
    }
    extern "C" fn mock_get_upvalue(_ctx: *mut u8, idx: i64) -> i64 {
        idx + 500
    }
    extern "C" fn mock_set_upvalue(_ctx: *mut u8, _idx: i64, val: i64) -> i64 {
        val
    }
    extern "C" fn mock_load_this(_ctx: *mut u8) -> i64 {
        0x7415_0000_i64
    }
    extern "C" fn mock_set_global(
        _ctx: *mut u8,
        _name: i64,
        val: i64,
        _ic: i64,
        _decl: i64,
    ) -> i64 {
        val
    }

    fn make_all_helpers() -> RuntimeHelpers {
        let mut h = RuntimeHelpers::new();
        unsafe {
            h.set(HelperKind::LoadConst, mock_load_const as *const u8);
            h.set(HelperKind::GetGlobal, mock_get_global as *const u8);
            h.set(HelperKind::SetGlobal, mock_set_global as *const u8);
            h.set(HelperKind::NewObject, mock_new_object as *const u8);
            h.set(HelperKind::NewArray, mock_new_array as *const u8);
            h.set(HelperKind::GetPropConst, mock_get_prop_const as *const u8);
            h.set(HelperKind::SetPropConst, mock_set_prop_const as *const u8);
            h.set(HelperKind::GetProp, mock_get_prop as *const u8);
            h.set(HelperKind::SetProp, mock_set_prop as *const u8);
            h.set(
                HelperKind::CallFunction,
                mock_call_function as *const u8,
            );
            h.set(
                HelperKind::CreateClosure,
                mock_create_closure as *const u8,
            );
            h.set(HelperKind::ThrowValue, mock_throw as *const u8);
            h.set(HelperKind::GetElem, mock_get_elem as *const u8);
            h.set(HelperKind::SetElem, mock_set_elem as *const u8);
            h.set(HelperKind::DefineProperty, mock_define_prop as *const u8);
            h.set(HelperKind::DeleteProp, mock_delete_prop as *const u8);
            h.set(HelperKind::GetUpvalue, mock_get_upvalue as *const u8);
            h.set(HelperKind::SetUpvalue, mock_set_upvalue as *const u8);
            h.set(HelperKind::LoadThis, mock_load_this as *const u8);
        }
        h
    }

    #[test]
    fn translation_load_const_via_helper() {
        let constants: Vec<i64> = vec![100, 200, 300];
        let ctx_data = (constants.as_ptr(), constants.len());
        let f = Function::builder()
            .name("lc")
            .register_count(1)
            .instruction(Instruction::LoadConst {
                dst: Register(0),
                idx: ConstantIndex(1),
            })
            .instruction(Instruction::Return { src: Register(0) })
            .build();
        let mut jit = JitCompiler::with_helpers(make_all_helpers()).expect("jit init");
        let a = jit.compile_function(&f).expect("compile");
        assert_eq!(
            jit.execute_with_ctx(a, &ctx_data as *const _ as *mut u8),
            200
        );
    }

    #[test]
    fn translation_get_global_via_helper() {
        let f = Function::builder()
            .name("gg")
            .register_count(1)
            .instruction(Instruction::GetGlobal {
                dst: Register(0),
                name: ConstantIndex(5),
                ic_index: 0,
            })
            .instruction(Instruction::Return { src: Register(0) })
            .build();
        let mut jit = JitCompiler::with_helpers(make_all_helpers()).expect("jit init");
        let a = jit.compile_function(&f).expect("compile");
        assert_eq!(jit.execute_compiled_i64(a), 500); // 5 * 100
    }

    #[test]
    fn translation_new_object_via_helper() {
        let f = Function::builder()
            .name("no")
            .register_count(1)
            .instruction(Instruction::NewObject { dst: Register(0) })
            .instruction(Instruction::Return { src: Register(0) })
            .build();
        let mut jit = JitCompiler::with_helpers(make_all_helpers()).expect("jit init");
        let a = jit.compile_function(&f).expect("compile");
        assert_eq!(jit.execute_compiled_i64(a), 0xDEAD_BEEF_i64);
    }

    #[test]
    fn translation_get_prop_const_via_helper() {
        let f = Function::builder()
            .name("gpc")
            .register_count(2)
            .instruction(Instruction::LoadInt32 {
                dst: Register(0),
                value: 1000,
            })
            .instruction(Instruction::GetPropConst {
                dst: Register(1),
                obj: Register(0),
                name: ConstantIndex(7),
                ic_index: 0,
            })
            .instruction(Instruction::Return { src: Register(1) })
            .build();
        let mut jit = JitCompiler::with_helpers(make_all_helpers()).expect("jit init");
        let a = jit.compile_function(&f).expect("compile");
        // obj is NaN-boxed 1000, name is 7, mock returns obj + name (raw addition)
        assert_eq!(jit.execute_compiled_i64(a), nb(1000) + 7); // mock arithmetic on raw i64
    }

    #[test]
    fn translation_call_function_via_helper() {
        let f = Function::builder()
            .name("call")
            .register_count(4)
            .instruction(Instruction::LoadInt32 {
                dst: Register(0),
                value: 999,
            }) // callee
            .instruction(Instruction::LoadInt32 {
                dst: Register(1),
                value: 10,
            }) // arg0
            .instruction(Instruction::LoadInt32 {
                dst: Register(2),
                value: 20,
            }) // arg1
            .instruction(Instruction::Call {
                dst: Register(3),
                func: Register(0),
                argc: 2,
            })
            .instruction(Instruction::Return { src: Register(3) })
            .build();
        let mut jit = JitCompiler::with_helpers(make_all_helpers()).expect("jit init");
        let a = jit.compile_function(&f).expect("compile");
        // mock_call_function returns first arg (NaN-boxed 10)
        assert_eq!(jit.execute_compiled_i64(a), nb(10));
    }

    #[test]
    fn translation_create_closure_via_helper() {
        let f = Function::builder()
            .name("cl")
            .register_count(1)
            .instruction(Instruction::Closure {
                dst: Register(0),
                func: FunctionIndex(3),
            })
            .instruction(Instruction::Return { src: Register(0) })
            .build();
        let mut jit = JitCompiler::with_helpers(make_all_helpers()).expect("jit init");
        let a = jit.compile_function(&f).expect("compile");
        assert_eq!(jit.execute_compiled_i64(a), 1003); // 3 + 1000
    }

    #[test]
    fn translation_new_array_via_helper() {
        let f = Function::builder()
            .name("na")
            .register_count(1)
            .instruction(Instruction::NewArray {
                dst: Register(0),
                len: 5,
            })
            .instruction(Instruction::Return { src: Register(0) })
            .build();
        let mut jit = JitCompiler::with_helpers(make_all_helpers()).expect("jit init");
        let a = jit.compile_function(&f).expect("compile");
        assert_eq!(jit.execute_compiled_i64(a), 0xBEEF_0000_i64 + 5);
    }

    #[test]
    fn translation_unsupported_without_helper() {
        let f = Function::builder()
            .name("noh")
            .register_count(1)
            .instruction(Instruction::NewObject { dst: Register(0) })
            .instruction(Instruction::Return { src: Register(0) })
            .build();
        let mut jit = JitCompiler::new().expect("jit init"); // no helpers
        let err = jit.compile_function(&f).expect_err("should fail");
        assert!(matches!(
            err,
            JitError::UnsupportedInstruction { pc: 0, .. }
        ));
    }

    // ===== Bailout mechanism tests =====

    #[test]
    fn bailout_on_f64_guard_failure_without_helper() {
        // With f64-only feedback and int32 inputs, the f64 guard will fail.
        // Without a generic helper, JIT should return BAILOUT_SENTINEL instead of trapping.
        use crate::bailout::BAILOUT_SENTINEL;

        let f = Function::builder()
            .name("bailout_f64")
            .register_count(3)
            .instruction(Instruction::LoadInt32 {
                dst: Register(0),
                value: 7,
            })
            .instruction(Instruction::LoadInt32 {
                dst: Register(1),
                value: 3,
            })
            .instruction(Instruction::Add {
                dst: Register(2),
                lhs: Register(0),
                rhs: Register(1),
                feedback_index: 0,
            })
            .instruction(Instruction::Return { src: Register(2) })
            .feedback_vector_size(1)
            .build();
        set_feedback_f64_only(&f, 0);
        let mut jit = JitCompiler::new().expect("jit init");
        let a = jit.compile_function(&f).expect("compile");
        // f64 guard fails on int32 inputs → returns bailout sentinel
        let result = jit.execute_compiled_i64(a);
        assert_eq!(result, BAILOUT_SENTINEL);
        assert!(crate::is_bailout(result));
    }

    #[test]
    fn no_bailout_on_successful_guard() {
        // With int32 feedback and int32 inputs, the guard succeeds → normal return.
        use crate::bailout::BAILOUT_SENTINEL;

        let f = Function::builder()
            .name("no_bailout")
            .register_count(3)
            .instruction(Instruction::LoadInt32 {
                dst: Register(0),
                value: 10,
            })
            .instruction(Instruction::LoadInt32 {
                dst: Register(1),
                value: 20,
            })
            .instruction(Instruction::Add {
                dst: Register(2),
                lhs: Register(0),
                rhs: Register(1),
                feedback_index: 0,
            })
            .instruction(Instruction::Return { src: Register(2) })
            .feedback_vector_size(1)
            .build();
        set_feedback_int32_only(&f, 0);
        let mut jit = JitCompiler::new().expect("jit init");
        let a = jit.compile_function(&f).expect("compile");
        let result = jit.execute_compiled_i64(a);
        assert_ne!(result, BAILOUT_SENTINEL);
        assert_eq!(result, nb(30));
    }

    #[test]
    fn bailout_count_tracking() {
        use crate::DEOPT_THRESHOLD;

        let f = Function::builder()
            .name("track_bailouts")
            .register_count(1)
            .instruction(Instruction::Return { src: Register(0) })
            .build();

        assert_eq!(f.get_bailout_count(), 0);
        assert!(!f.is_deoptimized());

        // Record bailouts below threshold
        for _ in 0..(DEOPT_THRESHOLD - 1) {
            assert!(!f.record_bailout(DEOPT_THRESHOLD));
        }
        assert_eq!(f.get_bailout_count(), DEOPT_THRESHOLD - 1);
        assert!(!f.is_deoptimized());

        // This one crosses the threshold → deoptimized
        assert!(f.record_bailout(DEOPT_THRESHOLD));
        assert!(f.is_deoptimized());
        assert_eq!(f.get_bailout_count(), DEOPT_THRESHOLD);

        // Further bailouts don't trigger deopt again (already deoptimized)
        assert!(!f.record_bailout(DEOPT_THRESHOLD));
    }

    #[test]
    fn bailout_on_unary_guard_failure() {
        // Neg with f64 input when compiled with i32 guard only (no generic helper)
        // should return BAILOUT_SENTINEL
        use crate::bailout::BAILOUT_SENTINEL;

        // Load a constant that looks like a float (raw f64 bits, not an int32)
        let f64_const: i64 = f64::to_bits(3.14) as i64;
        let constants: Vec<i64> = vec![f64_const];
        let ctx_data = (constants.as_ptr(), constants.len());

        let f = Function::builder()
            .name("bailout_neg")
            .register_count(2)
            .instruction(Instruction::LoadConst {
                dst: Register(0),
                idx: ConstantIndex(0),
            })
            .instruction(Instruction::Neg {
                dst: Register(1),
                src: Register(0),
            })
            .instruction(Instruction::Return { src: Register(1) })
            .build();

        // Use helpers for LoadConst but NOT for GenericNeg
        let mut h = RuntimeHelpers::new();
        unsafe {
            h.set(HelperKind::LoadConst, mock_load_const as *const u8);
        }
        let mut jit = JitCompiler::with_helpers(h).expect("jit init");
        let a = jit.compile_function(&f).expect("compile");

        let result = jit.execute_with_ctx(a, &ctx_data as *const _ as *mut u8);
        // f64 value fails i32 guard → bailout
        assert_eq!(result, BAILOUT_SENTINEL);
    }
}
