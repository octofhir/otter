//! Executable memory management for JIT-compiled code.
//!
//! Wraps Cranelift's `JITModule` to manage compiled function code.
//! Each compilation produces a `CompiledFunction` that can be called
//! via a function pointer.

use cranelift_codegen::Context as ClifContext;
use cranelift_codegen::ir::Function as ClifFunction;
use cranelift_codegen::isa::OwnedTargetIsa;
use cranelift_codegen::settings::{self, Configurable};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{Linkage, Module};

use crate::JitError;
use crate::abi::jit_function_signature;
use crate::context::JitContext;

/// A compiled function ready to execute.
pub struct CompiledFunction {
    /// The raw function pointer: `extern "C" fn(*mut JitContext) -> u64`
    pub entry: *const u8,
    /// Code size in bytes.
    pub code_size: usize,
    /// The JIT module that owns the code memory (must stay alive).
    _module: JITModule,
}

// SAFETY: The compiled code is immutable after finalization.
unsafe impl Send for CompiledFunction {}
unsafe impl Sync for CompiledFunction {}

impl CompiledFunction {
    /// Call this compiled function with the given JitContext.
    ///
    /// # Safety
    /// The caller must ensure:
    /// - `ctx` is a valid, fully initialized JitContext
    /// - The compiled code matches the function this context was set up for
    pub unsafe fn call(&self, ctx: &mut JitContext) -> u64 {
        let func: unsafe extern "C" fn(*mut JitContext) -> u64 =
            unsafe { std::mem::transmute(self.entry) };
        unsafe { func(ctx) }
    }
}

/// Create the Cranelift target ISA for the current host platform.
pub fn create_host_isa() -> Result<OwnedTargetIsa, JitError> {
    let mut flag_builder = settings::builder();
    flag_builder
        .set("use_colocated_libcalls", "false")
        .map_err(|e| JitError::Cranelift(e.to_string()))?;
    flag_builder
        .set("is_pic", "false")
        .map_err(|e| JitError::Cranelift(e.to_string()))?;
    // Enable Cranelift optimizations: GVN, LICM, constant folding, etc.
    flag_builder
        .set("opt_level", "speed")
        .map_err(|e| JitError::Cranelift(e.to_string()))?;

    let isa_builder =
        cranelift_native::builder().map_err(|e| JitError::Cranelift(e.to_string()))?;
    let flags = settings::Flags::new(flag_builder);
    isa_builder
        .finish(flags)
        .map_err(|e| JitError::Cranelift(e.to_string()))
}

/// Compile a Cranelift IR function into executable machine code.
///
/// `helper_symbols` are (name, fn_ptr) pairs registered in the JITModule
/// so that compiled code can call runtime helpers.
pub fn compile_clif_function(
    clif_func: ClifFunction,
    isa: OwnedTargetIsa,
    helper_symbols: &[(&str, *const u8)],
) -> Result<CompiledFunction, JitError> {
    let mut builder = JITBuilder::with_isa(isa.clone(), cranelift_module::default_libcall_names());

    // Register helper function symbols so compiled code can call them.
    for &(name, ptr) in helper_symbols {
        builder.symbol(name, ptr);
    }

    let mut module = JITModule::new(builder);

    let call_conv = isa.default_call_conv();
    let pointer_type = isa.pointer_type();

    // Declare the function in the module.
    let sig = jit_function_signature(call_conv, pointer_type);
    let func_id = module
        .declare_function("jit_entry", Linkage::Local, &sig)
        .map_err(|e| JitError::Cranelift(e.to_string()))?;

    // Dump CLIF IR before compilation if requested.
    if crate::config::JIT_CONFIG.dump_asm {
        eprintln!("[JIT] === CLIF IR ===");
        eprintln!("{}", clif_func.display());
    }

    // Compile and define.
    let mut ctx = ClifContext::for_function(clif_func);
    module
        .define_function(func_id, &mut ctx)
        .map_err(|e| JitError::Cranelift(e.to_string()))?;

    // Dump compiled code disassembly if requested.
    if crate::config::JIT_CONFIG.dump_asm {
        if let Some(compiled) = ctx.compiled_code() {
            // Dump the VCode (Cranelift's near-machine-code representation)
            if let Some(vcode) = compiled.vcode.as_ref() {
                eprintln!("[JIT] === VCode (near-asm) ===");
                eprintln!("{vcode}");
            }
            // Dump raw code bytes for external disassembly
            let code = compiled.code_buffer();
            let total_size = compiled.code_info().total_size;
            eprintln!("[JIT] === Native code: {} bytes ===", total_size);
            // Print hex dump for llvm-objdump -d
            for (i, chunk) in code.iter().enumerate() {
                if i % 16 == 0 {
                    if i > 0 {
                        eprintln!();
                    }
                    eprint!("  {:04x}: ", i);
                }
                eprint!("{:02x} ", chunk);
            }
            eprintln!();
        }
    }

    // Finalize — make the code executable.
    module
        .finalize_definitions()
        .map_err(|e| JitError::Cranelift(e.to_string()))?;

    let code_ptr = module.get_finalized_function(func_id);
    let code_size = ctx.compiled_code().unwrap().code_info().total_size as usize;

    if crate::config::JIT_CONFIG.dump_asm {
        eprintln!("[JIT] compiled function at {:p}, {} bytes", code_ptr, code_size);
    }

    Ok(CompiledFunction {
        entry: code_ptr,
        code_size,
        _module: module,
    })
}
