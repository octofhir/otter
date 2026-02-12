//! Baseline JIT compiler wrapper around Cranelift.

use cranelift_codegen::ir::{AbiParam, UserFuncName, types};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{Linkage, Module, ModuleError, default_libcall_names};
use otter_vm_bytecode::Function;

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
}

impl JitCompiler {
    /// Create a new baseline JIT compiler instance.
    pub fn new() -> Result<Self, JitError> {
        let builder = JITBuilder::new(default_libcall_names())
            .map_err(|e| JitError::Builder(e.to_string()))?;
        let module = JITModule::new(builder);
        Ok(Self {
            module,
            function_builder_ctx: FunctionBuilderContext::new(),
            context: cranelift_codegen::Context::new(),
            next_function_id: 0,
        })
    }

    /// Compile a bytecode function into native code.
    ///
    /// Baseline translation currently supports a subset of bytecode instructions.
    /// Unsupported instructions return `JitError::UnsupportedInstruction`.
    pub fn compile_function(
        &mut self,
        function: &Function,
    ) -> Result<JitCompileArtifact, JitError> {
        let mut signature = self.module.make_signature();
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

        {
            let mut builder =
                FunctionBuilder::new(&mut self.context.func, &mut self.function_builder_ctx);
            translator::translate_function(&mut builder, function)?;
            builder.finalize();
        }

        self.module.define_function(func_id, &mut self.context)?;
        self.module.clear_context(&mut self.context);
        self.module.finalize_definitions()?;

        let code_ptr = self.module.get_finalized_function(func_id);
        Ok(JitCompileArtifact { code_ptr })
    }

    /// Execute a compiled artifact as `extern "C" fn() -> i64`.
    ///
    /// This is intended for translator unit tests.
    pub fn execute_compiled_i64(&self, artifact: JitCompileArtifact) -> i64 {
        let func: extern "C" fn() -> i64 = unsafe {
            // SAFETY: Artifacts are produced by this compiler with signature `() -> i64`.
            std::mem::transmute(artifact.code_ptr)
        };
        func()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use otter_vm_bytecode::{Instruction, JumpOffset, Register};

    #[test]
    fn basic_compile() {
        let function = Function::builder()
            .name("basic_compile")
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

        assert_eq!(jit.execute_compiled_i64(artifact), 5);
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

        assert_eq!(jit.execute_compiled_i64(artifact), 42);
    }

    #[test]
    fn translation_reports_unsupported_instruction() {
        let function = Function::builder()
            .name("translation_unsupported")
            .register_count(1)
            .instruction(Instruction::LoadUndefined { dst: Register(0) })
            .instruction(Instruction::Return { src: Register(0) })
            .build();

        let mut jit = JitCompiler::new().expect("jit initialization should succeed");
        let err = jit
            .compile_function(&function)
            .expect_err("unsupported bytecode should fail translation");

        match err {
            JitError::UnsupportedInstruction { pc, opcode } => {
                assert_eq!(pc, 0);
                assert_eq!(opcode, "LoadUndefined");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }
}
