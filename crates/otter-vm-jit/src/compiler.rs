//! Baseline JIT compiler wrapper around Cranelift.

use cranelift_codegen::ir::{AbiParam, UserFuncName, types};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{Linkage, Module, ModuleError, default_libcall_names};
use otter_vm_bytecode::{Constant, Function};

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
    UnsupportedInstruction { pc: usize, opcode: String },

    /// Jump target is outside the bytecode function bounds.
    #[error("invalid jump target from pc {pc} with offset {offset} (len={instruction_count})")]
    InvalidJumpTarget {
        pc: usize,
        offset: i32,
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
    helper_func_ids: Option<HelperFuncIds>,
}

impl JitCompiler {
    /// Create a new baseline JIT compiler instance (no runtime helpers).
    pub fn new() -> Result<Self, JitError> {
        let builder = JITBuilder::new(default_libcall_names())
            .map_err(|e| JitError::Builder(e.to_string()))?;
        let module = JITModule::new(builder);
        Ok(Self {
            module,
            function_builder_ctx: FunctionBuilderContext::new(),
            context: cranelift_codegen::Context::new(),
            next_function_id: 0,
            helper_func_ids: None,
        })
    }

    /// Create a JIT compiler with runtime helper support.
    ///
    /// Runtime helpers enable compilation of property access, function calls,
    /// and other complex operations that require VM context.
    pub fn new_with_helpers(helpers: RuntimeHelpers) -> Result<Self, JitError> {
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
            helper_func_ids: Some(helper_func_ids),
        })
    }

    /// Whether runtime helpers are available for compilation.
    pub fn has_helpers(&self) -> bool {
        self.helper_func_ids.is_some()
    }

    /// Compile a bytecode function into native code.
    ///
    /// Baseline translation currently supports a subset of bytecode instructions.
    /// Unsupported instructions return `JitError::UnsupportedInstruction`.
    pub fn compile_function(
        &mut self,
        function: &Function,
    ) -> Result<JitCompileArtifact, JitError> {
        self.compile_function_with_constants(function, &[])
    }

    /// Compile a bytecode function with constant pool access.
    pub fn compile_function_with_constants(
        &mut self,
        function: &Function,
        constants: &[Constant],
    ) -> Result<JitCompileArtifact, JitError> {
        let mut signature = self.module.make_signature();
        // Signature: (ctx: I64, args_ptr: I64, argc: I32) -> I64
        signature.params.push(AbiParam::new(types::I64)); // ctx pointer
        signature.params.push(AbiParam::new(types::I64)); // args_ptr
        signature.params.push(AbiParam::new(types::I32)); // argc
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

        // Declare helper function refs for this compilation if available
        let helper_refs = self.helper_func_ids.as_ref().map(|func_ids| {
            HelperRefs::declare(func_ids, &mut self.module, &mut self.context.func)
        });

        {
            let mut builder =
                FunctionBuilder::new(&mut self.context.func, &mut self.function_builder_ctx);
            translator::translate_function_with_constants(
                &mut builder,
                function,
                constants,
                helper_refs.as_ref(),
            )?;
            builder.finalize();
        }

        self.module.define_function(func_id, &mut self.context)?;
        self.module.clear_context(&mut self.context);
        self.module.finalize_definitions()?;

        let code_ptr = self.module.get_finalized_function(func_id);
        Ok(JitCompileArtifact { code_ptr })
    }

    /// Execute a compiled artifact as `extern "C" fn(*mut u8, *const i64, u32) -> i64`.
    ///
    /// This is intended for translator unit tests. Passes null ctx pointer.
    pub fn execute_compiled_i64(&self, artifact: JitCompileArtifact) -> i64 {
        self.execute_compiled_i64_with_args(artifact, &[])
    }

    /// Execute a compiled artifact with pre-boxed NaN-value arguments.
    ///
    /// Passes null ctx pointer (sufficient for arithmetic-only functions).
    pub fn execute_compiled_i64_with_args(
        &self,
        artifact: JitCompileArtifact,
        args: &[i64],
    ) -> i64 {
        let func: extern "C" fn(*mut u8, *const i64, u32) -> i64 = unsafe {
            // SAFETY: Artifacts are produced by this compiler with signature
            // `(*mut u8, *const i64, u32) -> i64`.
            std::mem::transmute(artifact.code_ptr)
        };
        func(std::ptr::null_mut(), args.as_ptr(), args.len() as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use otter_vm_bytecode::{
        Constant, ConstantIndex, Instruction, JumpOffset, LocalIndex, Register,
    };

    fn boxed_i32(n: i32) -> i64 {
        crate::type_guards::TAG_INT32 | ((n as u32) as i64)
    }

    fn boxed_bool(value: bool) -> i64 {
        if value {
            crate::type_guards::TAG_TRUE
        } else {
            crate::type_guards::TAG_FALSE
        }
    }

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

        let mut jit = JitCompiler::new().expect("jit initialization should succeed");
        let artifact = jit
            .compile_function(&function)
            .expect("function compilation should succeed");

        assert!(!artifact.code_ptr.is_null());
    }

    #[test]
    fn translation_arithmetic_returns_value() {
        let function = Function::builder()
            .name("translation_arithmetic")
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

        let mut jit = JitCompiler::new().expect("jit initialization should succeed");
        let artifact = jit
            .compile_function(&function)
            .expect("translation should succeed");

        assert_eq!(jit.execute_compiled_i64(artifact), boxed_i32(5));
    }

    #[test]
    fn translation_control_flow_jump_if_true() {
        let function = Function::builder()
            .name("translation_control_flow")
            .register_count(3)
            .instruction(Instruction::LoadInt32 {
                dst: Register(0),
                value: 1,
            }) // pc 0
            .instruction(Instruction::JumpIfTrue {
                cond: Register(0),
                offset: JumpOffset(3),
            }) // pc 1 -> pc 4
            .instruction(Instruction::LoadInt32 {
                dst: Register(1),
                value: 7,
            }) // pc 2
            .instruction(Instruction::Return { src: Register(1) }) // pc 3
            .instruction(Instruction::LoadInt32 {
                dst: Register(2),
                value: 42,
            }) // pc 4
            .instruction(Instruction::Return { src: Register(2) }) // pc 5
            .build();

        let mut jit = JitCompiler::new().expect("jit initialization should succeed");
        let artifact = jit
            .compile_function(&function)
            .expect("translation should succeed");

        assert_eq!(jit.execute_compiled_i64(artifact), boxed_i32(42));
    }

    #[test]
    fn translation_locals_get_set_roundtrip() {
        let function = Function::builder()
            .name("translation_locals")
            .register_count(2)
            .local_count(1)
            .instruction(Instruction::LoadInt32 {
                dst: Register(0),
                value: 11,
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

        let mut jit = JitCompiler::new().expect("jit initialization should succeed");
        let artifact = jit
            .compile_function(&function)
            .expect("translation should succeed");

        assert_eq!(jit.execute_compiled_i64(artifact), boxed_i32(11));
    }

    #[test]
    fn translation_reads_param_from_argv() {
        let function = Function::builder()
            .name("translation_param_from_argv")
            .param_count(1)
            .local_count(1)
            .register_count(3)
            .instruction(Instruction::GetLocal {
                dst: Register(0),
                idx: LocalIndex(0),
            })
            .instruction(Instruction::LoadInt32 {
                dst: Register(1),
                value: 1,
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

        let mut jit = JitCompiler::new().expect("jit initialization should succeed");
        let artifact = jit
            .compile_function(&function)
            .expect("translation should succeed");

        let argv = [boxed_i32(41)];
        assert_eq!(
            jit.execute_compiled_i64_with_args(artifact, &argv),
            boxed_i32(42)
        );
    }

    #[test]
    fn translation_control_flow_jump_if_nullish() {
        let function = Function::builder()
            .name("translation_jump_if_nullish")
            .register_count(3)
            .instruction(Instruction::LoadNull { dst: Register(0) }) // pc 0
            .instruction(Instruction::JumpIfNullish {
                src: Register(0),
                offset: JumpOffset(3),
            }) // pc 1 -> pc 4
            .instruction(Instruction::LoadInt32 {
                dst: Register(1),
                value: 7,
            }) // pc 2
            .instruction(Instruction::Return { src: Register(1) }) // pc 3
            .instruction(Instruction::LoadInt32 {
                dst: Register(2),
                value: 42,
            }) // pc 4
            .instruction(Instruction::Return { src: Register(2) }) // pc 5
            .build();

        let mut jit = JitCompiler::new().expect("jit initialization should succeed");
        let artifact = jit
            .compile_function(&function)
            .expect("translation should succeed");

        assert_eq!(jit.execute_compiled_i64(artifact), boxed_i32(42));
    }

    #[test]
    fn translation_load_const_number() {
        let function = Function::builder()
            .name("translation_load_const_number")
            .register_count(1)
            .instruction(Instruction::LoadConst {
                dst: Register(0),
                idx: ConstantIndex(0),
            })
            .instruction(Instruction::Return { src: Register(0) })
            .build();

        let constants = [Constant::number(123.5)];
        let mut jit = JitCompiler::new().expect("jit initialization should succeed");
        let artifact = jit
            .compile_function_with_constants(&function, &constants)
            .expect("translation should succeed");

        assert_eq!(
            jit.execute_compiled_i64(artifact),
            123.5_f64.to_bits() as i64
        );
    }

    #[test]
    fn translation_add_f64_fast_path() {
        let function = Function::builder()
            .name("translation_add_f64_fast_path")
            .register_count(3)
            .instruction(Instruction::LoadConst {
                dst: Register(0),
                idx: ConstantIndex(0),
            })
            .instruction(Instruction::LoadConst {
                dst: Register(1),
                idx: ConstantIndex(1),
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
        // Set feedback: f64 operands observed
        function.feedback_vector.write()[0].type_observations.observe_number();

        let constants = [Constant::number(1.5), Constant::number(2.25)];
        let mut jit = JitCompiler::new().expect("jit initialization should succeed");
        let artifact = jit
            .compile_function_with_constants(&function, &constants)
            .expect("translation should succeed");

        assert_eq!(
            jit.execute_compiled_i64(artifact),
            3.75_f64.to_bits() as i64
        );
    }

    #[test]
    fn translation_add_mixed_numeric_fast_path() {
        let function = Function::builder()
            .name("translation_add_mixed_numeric_fast_path")
            .register_count(3)
            .instruction(Instruction::LoadInt32 {
                dst: Register(0),
                value: 1,
            })
            .instruction(Instruction::LoadConst {
                dst: Register(1),
                idx: ConstantIndex(0),
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
        // Set feedback: mixed int32 + number observed
        function.feedback_vector.write()[0].type_observations.observe_int32();
        function.feedback_vector.write()[0].type_observations.observe_number();

        let constants = [Constant::number(2.5)];
        let mut jit = JitCompiler::new().expect("jit initialization should succeed");
        let artifact = jit
            .compile_function_with_constants(&function, &constants)
            .expect("translation should succeed");

        assert_eq!(jit.execute_compiled_i64(artifact), 3.5_f64.to_bits() as i64);
    }

    #[test]
    fn translation_div_int32_non_exact_uses_numeric_fast_path() {
        let function = Function::builder()
            .name("translation_div_int32_non_exact_uses_numeric_fast_path")
            .register_count(3)
            .instruction(Instruction::LoadInt32 {
                dst: Register(0),
                value: 7,
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

        let mut jit = JitCompiler::new().expect("jit initialization should succeed");
        let artifact = jit
            .compile_function(&function)
            .expect("translation should succeed");

        assert_eq!(jit.execute_compiled_i64(artifact), 3.5_f64.to_bits() as i64);
    }

    #[test]
    fn translation_add_int32_overflow_uses_numeric_fast_path() {
        let function = Function::builder()
            .name("translation_add_int32_overflow_uses_numeric_fast_path")
            .register_count(3)
            .instruction(Instruction::LoadInt32 {
                dst: Register(0),
                value: i32::MAX,
            })
            .instruction(Instruction::LoadInt32 {
                dst: Register(1),
                value: 1,
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
        // Set feedback: both int32 and number observed (overflow produces f64)
        function.feedback_vector.write()[0].type_observations.observe_int32();
        function.feedback_vector.write()[0].type_observations.observe_number();

        let mut jit = JitCompiler::new().expect("jit initialization should succeed");
        let artifact = jit
            .compile_function(&function)
            .expect("translation should succeed");

        assert_eq!(
            jit.execute_compiled_i64(artifact),
            2147483648.0_f64.to_bits() as i64
        );
    }

    #[test]
    fn translation_div_by_zero_uses_numeric_fast_path() {
        let function = Function::builder()
            .name("translation_div_by_zero_uses_numeric_fast_path")
            .register_count(3)
            .instruction(Instruction::LoadInt32 {
                dst: Register(0),
                value: 1,
            })
            .instruction(Instruction::LoadInt32 {
                dst: Register(1),
                value: 0,
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

        let mut jit = JitCompiler::new().expect("jit initialization should succeed");
        let artifact = jit
            .compile_function(&function)
            .expect("translation should succeed");

        assert_eq!(
            jit.execute_compiled_i64(artifact),
            f64::INFINITY.to_bits() as i64
        );
    }

    #[test]
    fn translation_lt_f64_fast_path() {
        let function = Function::builder()
            .name("translation_lt_f64_fast_path")
            .register_count(3)
            .instruction(Instruction::LoadConst {
                dst: Register(0),
                idx: ConstantIndex(0),
            })
            .instruction(Instruction::LoadConst {
                dst: Register(1),
                idx: ConstantIndex(1),
            })
            .instruction(Instruction::Lt {
                dst: Register(2),
                lhs: Register(0),
                rhs: Register(1),
            })
            .instruction(Instruction::Return { src: Register(2) })
            .build();

        let constants = [Constant::number(1.5), Constant::number(2.25)];
        let mut jit = JitCompiler::new().expect("jit initialization should succeed");
        let artifact = jit
            .compile_function_with_constants(&function, &constants)
            .expect("translation should succeed");

        assert_eq!(jit.execute_compiled_i64(artifact), boxed_bool(true));
    }

    #[test]
    fn translation_lt_mixed_numeric_fast_path() {
        let function = Function::builder()
            .name("translation_lt_mixed_numeric_fast_path")
            .register_count(3)
            .instruction(Instruction::LoadInt32 {
                dst: Register(0),
                value: 2,
            })
            .instruction(Instruction::LoadConst {
                dst: Register(1),
                idx: ConstantIndex(0),
            })
            .instruction(Instruction::Lt {
                dst: Register(2),
                lhs: Register(0),
                rhs: Register(1),
            })
            .instruction(Instruction::Return { src: Register(2) })
            .build();

        let constants = [Constant::number(2.5)];
        let mut jit = JitCompiler::new().expect("jit initialization should succeed");
        let artifact = jit
            .compile_function_with_constants(&function, &constants)
            .expect("translation should succeed");

        assert_eq!(jit.execute_compiled_i64(artifact), boxed_bool(true));
    }

    #[test]
    fn translation_bitor_numeric_to_int32_fast_path() {
        let function = Function::builder()
            .name("translation_bitor_numeric_to_int32_fast_path")
            .register_count(3)
            .instruction(Instruction::LoadConst {
                dst: Register(0),
                idx: ConstantIndex(0),
            })
            .instruction(Instruction::LoadInt32 {
                dst: Register(1),
                value: 0,
            })
            .instruction(Instruction::BitOr {
                dst: Register(2),
                lhs: Register(0),
                rhs: Register(1),
            })
            .instruction(Instruction::Return { src: Register(2) })
            .build();

        let constants = [Constant::number(4_294_967_297.0)];
        let mut jit = JitCompiler::new().expect("jit initialization should succeed");
        let artifact = jit
            .compile_function_with_constants(&function, &constants)
            .expect("translation should succeed");

        assert_eq!(jit.execute_compiled_i64(artifact), boxed_i32(1));
    }

    #[test]
    fn translation_bitnot_numeric_to_int32_fast_path() {
        let function = Function::builder()
            .name("translation_bitnot_numeric_to_int32_fast_path")
            .register_count(2)
            .instruction(Instruction::LoadConst {
                dst: Register(0),
                idx: ConstantIndex(0),
            })
            .instruction(Instruction::BitNot {
                dst: Register(1),
                src: Register(0),
            })
            .instruction(Instruction::Return { src: Register(1) })
            .build();

        let constants = [Constant::number(1.9)];
        let mut jit = JitCompiler::new().expect("jit initialization should succeed");
        let artifact = jit
            .compile_function_with_constants(&function, &constants)
            .expect("translation should succeed");

        assert_eq!(jit.execute_compiled_i64(artifact), boxed_i32(-2));
    }

    #[test]
    fn translation_ushr_high_bit_returns_number() {
        let function = Function::builder()
            .name("translation_ushr_high_bit_returns_number")
            .register_count(3)
            .instruction(Instruction::LoadConst {
                dst: Register(0),
                idx: ConstantIndex(0),
            })
            .instruction(Instruction::LoadInt32 {
                dst: Register(1),
                value: 0,
            })
            .instruction(Instruction::Ushr {
                dst: Register(2),
                lhs: Register(0),
                rhs: Register(1),
            })
            .instruction(Instruction::Return { src: Register(2) })
            .build();

        let constants = [Constant::number(-1.0)];
        let mut jit = JitCompiler::new().expect("jit initialization should succeed");
        let artifact = jit
            .compile_function_with_constants(&function, &constants)
            .expect("translation should succeed");

        assert_eq!(
            jit.execute_compiled_i64(artifact),
            4_294_967_295.0_f64.to_bits() as i64
        );
    }

    #[test]
    fn translation_strict_eq_mixed_numeric_zero() {
        let function = Function::builder()
            .name("translation_strict_eq_mixed_numeric_zero")
            .register_count(3)
            .instruction(Instruction::LoadInt32 {
                dst: Register(0),
                value: 0,
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

        let constants = [Constant::number(-0.0)];
        let mut jit = JitCompiler::new().expect("jit initialization should succeed");
        let artifact = jit
            .compile_function_with_constants(&function, &constants)
            .expect("translation should succeed");

        assert_eq!(jit.execute_compiled_i64(artifact), boxed_bool(true));
    }

    #[test]
    fn translation_reports_unsupported_instruction() {
        let function = Function::builder()
            .name("translation_unsupported")
            .register_count(1)
            .instruction(Instruction::LoadConst {
                dst: Register(0),
                idx: ConstantIndex(0),
            })
            .instruction(Instruction::Return { src: Register(0) })
            .build();

        let mut jit = JitCompiler::new().expect("jit initialization should succeed");
        let err = jit
            .compile_function(&function)
            .expect_err("unsupported bytecode should fail translation");

        match err {
            JitError::UnsupportedInstruction { pc, opcode } => {
                assert_eq!(pc, 0);
                assert_eq!(opcode, "LoadConst");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }
}
