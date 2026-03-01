//! Baseline JIT compiler wrapper around Cranelift.

use cranelift_codegen::ir::{AbiParam, UserFuncName, types};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{Linkage, Module, ModuleError, default_libcall_names};
use otter_vm_bytecode::instruction::Instruction;
use otter_vm_bytecode::{Constant, Function};

use crate::runtime_helpers::{HelperFuncIds, HelperRefs, RuntimeHelpers};
use crate::translator;

/// Result of compiling a bytecode function to native code.
#[derive(Debug, Clone, Copy)]
pub struct JitCompileArtifact {
    /// Entry pointer for compiled native code.
    pub code_ptr: *const u8,
}

/// Metadata for a single deopt-capable bytecode site.
///
/// # GC Safety (C1 invariant)
///
/// This struct must contain only plain scalar types and index containers.
/// No `GcRef`, `Value`, `JsObject`, `JsString`, or other GC-managed types
/// are permitted. This ensures deopt metadata can be stored, cloned, and
/// transmitted across threads without interacting with GC rooting.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeoptResumeSite {
    /// Bytecode program counter.
    pub bytecode_pc: u32,
    /// Native code offset for the deopt point. Not wired yet.
    pub native_offset: Option<u32>,
    /// Live virtual register indices required for precise resume.
    pub live_registers: Vec<u16>,
    /// Live local indices required for precise resume.
    pub live_locals: Vec<u16>,
}

/// Compile-time deopt metadata scaffold for a function.
///
/// # GC Safety (C1 invariant)
///
/// Contains only scalar metadata. See [`DeoptResumeSite`] for invariant details.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeoptMetadata {
    /// Sorted list of deopt-capable sites.
    pub sites: Vec<DeoptResumeSite>,
}

// Compile-time GC safety assertions: ensure deopt types are Send + Sync
// (impossible if they contained GcRef which is !Send + !Sync).
const _: () = {
    fn assert_send_sync<T: Send + Sync>() {}
    fn check() {
        assert_send_sync::<DeoptResumeSite>();
        assert_send_sync::<DeoptMetadata>();
    }
};

impl DeoptMetadata {
    /// Check whether metadata has a site for the provided bytecode pc.
    pub fn has_site(&self, pc: u32) -> bool {
        self.sites.iter().any(|site| site.bytecode_pc == pc)
    }
}

fn instruction_can_deopt(instruction: &Instruction) -> bool {
    !matches!(
        instruction,
        Instruction::LoadUndefined { .. }
            | Instruction::LoadNull { .. }
            | Instruction::LoadTrue { .. }
            | Instruction::LoadFalse { .. }
            | Instruction::LoadInt8 { .. }
            | Instruction::LoadInt32 { .. }
            | Instruction::LoadConst { .. }
            | Instruction::GetLocal { .. }
            | Instruction::SetLocal { .. }
            | Instruction::Move { .. }
            | Instruction::Jump { .. }
            | Instruction::JumpIfTrue { .. }
            | Instruction::JumpIfFalse { .. }
            | Instruction::JumpIfNullish { .. }
            | Instruction::JumpIfNotNullish { .. }
            | Instruction::Return { .. }
            | Instruction::ReturnUndefined
            | Instruction::Nop
    )
}

fn build_deopt_metadata(function: &Function) -> DeoptMetadata {
    let mut sites = Vec::new();
    for (pc, instruction) in function.instructions.read().iter().enumerate() {
        if instruction_can_deopt(instruction) {
            sites.push(DeoptResumeSite {
                bytecode_pc: pc as u32,
                native_offset: None,
                live_registers: Vec::new(),
                live_locals: Vec::new(),
            });
        }
    }
    DeoptMetadata { sites }
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
        let (artifact, _) =
            self.compile_function_with_constants_and_metadata(function, constants)?;
        Ok(artifact)
    }

    /// Compile a bytecode function and return JIT artifact plus deopt metadata scaffold.
    pub fn compile_function_with_constants_and_metadata(
        &mut self,
        function: &Function,
        constants: &[Constant],
    ) -> Result<(JitCompileArtifact, DeoptMetadata), JitError> {
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
        let metadata = build_deopt_metadata(function);
        Ok((JitCompileArtifact { code_ptr }, metadata))
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
    fn deopt_metadata_marks_only_deopt_capable_sites() {
        let function = Function::builder()
            .name("deopt_metadata_sites")
            .register_count(3)
            .instruction(Instruction::LoadInt32 {
                dst: Register(0),
                value: 1,
            }) // pc 0 (no deopt)
            .instruction(Instruction::LoadInt32 {
                dst: Register(1),
                value: 2,
            }) // pc 1 (no deopt)
            .instruction(Instruction::Add {
                dst: Register(2),
                lhs: Register(0),
                rhs: Register(1),
                feedback_index: 0,
            }) // pc 2 (can deopt)
            .instruction(Instruction::Return { src: Register(2) }) // pc 3 (no deopt)
            .feedback_vector_size(1)
            .build();

        let metadata = build_deopt_metadata(&function);
        assert_eq!(metadata.sites.len(), 1);
        assert_eq!(metadata.sites[0].bytecode_pc, 2);
        assert!(metadata.has_site(2));
        assert!(!metadata.has_site(0));
    }

    #[test]
    fn compile_with_metadata_returns_scaffold_sites() {
        let function = Function::builder()
            .name("compile_with_metadata")
            .register_count(1)
            .instruction(Instruction::LoadInt32 {
                dst: Register(0),
                value: 7,
            })
            .instruction(Instruction::Return { src: Register(0) })
            .build();

        let mut jit = JitCompiler::new().expect("jit initialization should succeed");
        let (artifact, metadata) = jit
            .compile_function_with_constants_and_metadata(&function, &[])
            .expect("function compilation with metadata should succeed");

        assert!(!artifact.code_ptr.is_null());
        assert!(metadata.sites.is_empty());
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
        function.feedback_vector.write()[0]
            .type_observations
            .observe_number();

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
        function.feedback_vector.write()[0]
            .type_observations
            .observe_int32();
        function.feedback_vector.write()[0]
            .type_observations
            .observe_number();

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
        function.feedback_vector.write()[0]
            .type_observations
            .observe_int32();
        function.feedback_vector.write()[0]
            .type_observations
            .observe_number();

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

    #[test]
    fn deopt_state_dump_captures_locals_and_registers_on_bailout() {
        // This function adds two non-int32 values (f64), causing a bailout on the Add
        // if we set the feedback vector to expect only int32. This tests that
        // the deopt path dumps local/register state to the context buffer.
        //
        // Layout: local[0] = param a, reg[0] = loaded int, reg[1] = Add result
        // Bailout happens at pc 2 (Add) when type guard fails.

        let function = Function::builder()
            .name("deopt_state_dump_test")
            .param_count(1)
            .local_count(1)
            .register_count(3)
            .instruction(Instruction::GetLocal {
                dst: Register(0),
                idx: LocalIndex(0),
            }) // pc 0
            .instruction(Instruction::LoadInt32 {
                dst: Register(1),
                value: 10,
            }) // pc 1
            .instruction(Instruction::Add {
                dst: Register(2),
                lhs: Register(0),
                rhs: Register(1),
                feedback_index: 0,
            }) // pc 2 - may bailout
            .instruction(Instruction::Return { src: Register(2) }) // pc 3
            .feedback_vector_size(1)
            .build();
        // Only int32 observed → JIT generates int32-only fast path
        function.feedback_vector.write()[0]
            .type_observations
            .observe_int32();

        let mut jit = JitCompiler::new().expect("jit init");
        let artifact = jit
            .compile_function(&function)
            .expect("compilation should succeed");

        // Set up a fake JitContext with deopt buffers
        let mut deopt_locals = vec![0_i64; 1]; // 1 local
        let mut deopt_regs = vec![0_i64; 3]; // 3 registers

        // Build a minimal context-like buffer matching JitContext layout.
        // The context is 136 bytes (through deopt_regs_count).
        let mut ctx_buf = vec![0_u8; 136];
        // Write deopt_locals_ptr at offset 104
        let locals_ptr = deopt_locals.as_mut_ptr() as u64;
        ctx_buf[104..112].copy_from_slice(&locals_ptr.to_ne_bytes());
        // Write deopt_locals_count at offset 112
        ctx_buf[112..116].copy_from_slice(&1_u32.to_ne_bytes());
        // Write deopt_regs_ptr at offset 120
        let regs_ptr = deopt_regs.as_mut_ptr() as u64;
        ctx_buf[120..128].copy_from_slice(&regs_ptr.to_ne_bytes());
        // Write deopt_regs_count at offset 128
        ctx_buf[128..132].copy_from_slice(&3_u32.to_ne_bytes());
        // Write bailout_pc = -1 at offset 96 (so we can detect if it was written)
        ctx_buf[96..104].copy_from_slice(&(-1_i64).to_ne_bytes());

        // Call with a f64 value (1.5) which will fail the int32 type guard → bailout
        let argv = [1.5_f64.to_bits() as i64];

        let func: extern "C" fn(*mut u8, *const i64, u32) -> i64 = unsafe {
            std::mem::transmute(artifact.code_ptr)
        };
        let result = func(ctx_buf.as_mut_ptr(), argv.as_ptr(), 1);

        // Should have bailed out
        assert_eq!(result, crate::bailout::BAILOUT_SENTINEL);

        // Check bailout_pc was written (offset 96)
        let bailout_pc = i64::from_ne_bytes(ctx_buf[96..104].try_into().unwrap());
        assert_eq!(bailout_pc, 2, "bailout should happen at pc 2 (Add)");

        // Check deopt locals: local[0] should contain the f64 value 1.5
        assert_eq!(
            deopt_locals[0] as u64,
            1.5_f64.to_bits(),
            "local[0] should contain the f64 param value"
        );

        // Check deopt registers: reg[0] should contain the f64 param, reg[1] should be int32(10)
        assert_eq!(
            deopt_regs[0] as u64,
            1.5_f64.to_bits(),
            "reg[0] should contain GetLocal result (f64 1.5)"
        );
        assert_eq!(
            deopt_regs[1],
            boxed_i32(10),
            "reg[1] should contain LoadInt32 value 10"
        );
    }
}
