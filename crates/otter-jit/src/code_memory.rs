//! Executable memory management for JIT-compiled code.
//!
//! Wraps Cranelift's `JITModule` to manage compiled function code.
//! Each compilation produces a `CompiledFunction` that can be called
//! via a function pointer.

use std::ptr::NonNull;

use cranelift_codegen::Context as ClifContext;
use cranelift_codegen::ir::Function as ClifFunction;
use cranelift_codegen::isa::OwnedTargetIsa;
use cranelift_codegen::settings::{self, Configurable};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{Linkage, Module};

use crate::arch::CodeBuffer;
use crate::JitError;
use crate::abi::jit_function_signature;
use crate::context::JitContext;

/// Actual code generation backend that produced a compiled function.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompiledCodeOrigin {
    /// The legacy Tier 1/Tier 2 Cranelift pipeline.
    MirBaseline,
    /// The direct template-baseline `bytecode -> asm stencil` path.
    TemplateBaseline,
}

enum CompiledCodeOwner {
    Cranelift { _module: Box<JITModule> },
    Executable { _buffer: ExecutableBuffer },
}

/// Executable memory that owns a copied machine-code buffer.
struct ExecutableBuffer {
    base: NonNull<u8>,
    map_len: usize,
}

impl ExecutableBuffer {
    #[cfg(unix)]
    fn from_code_buffer(buf: &CodeBuffer) -> Result<Self, JitError> {
        if !buf.relocations().is_empty() {
            return Err(JitError::Internal(
                "template stencil relocation install is not implemented yet".to_string(),
            ));
        }
        if buf.is_empty() {
            return Err(JitError::Internal(
                "refusing to install an empty code buffer".to_string(),
            ));
        }

        let map_len = round_up_to_page_size(buf.len())?;
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                map_len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANON,
                -1,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(JitError::Internal(format!(
                "mmap executable buffer failed: {}",
                std::io::Error::last_os_error()
            )));
        }

        let base = NonNull::new(ptr.cast::<u8>()).ok_or_else(|| {
            JitError::Internal("mmap returned a null executable buffer".to_string())
        })?;

        unsafe {
            std::ptr::copy_nonoverlapping(buf.bytes().as_ptr(), base.as_ptr(), buf.len());
            flush_instruction_cache(base.as_ptr(), buf.len());
            if libc::mprotect(base.as_ptr().cast(), map_len, libc::PROT_READ | libc::PROT_EXEC)
                != 0
            {
                let err = std::io::Error::last_os_error();
                libc::munmap(base.as_ptr().cast(), map_len);
                return Err(JitError::Internal(format!(
                    "mprotect executable buffer failed: {err}"
                )));
            }
        }

        Ok(Self { base, map_len })
    }

    #[cfg(not(unix))]
    fn from_code_buffer(_buf: &CodeBuffer) -> Result<Self, JitError> {
        Err(JitError::Internal(
            "template executable install is only implemented on unix hosts".to_string(),
        ))
    }

    fn entry(&self) -> *const u8 {
        self.base.as_ptr().cast_const()
    }
}

#[cfg(unix)]
fn round_up_to_page_size(len: usize) -> Result<usize, JitError> {
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if page_size <= 0 {
        return Err(JitError::Internal(
            "failed to query host page size for executable install".to_string(),
        ));
    }
    let page_size = page_size as usize;
    len.checked_add(page_size - 1)
        .map(|n| n / page_size * page_size)
        .ok_or_else(|| JitError::Internal("executable buffer size overflow".to_string()))
}

#[cfg(all(unix, target_arch = "aarch64", target_vendor = "apple"))]
unsafe fn flush_instruction_cache(ptr: *mut u8, len: usize) {
    unsafe extern "C" {
        fn sys_icache_invalidate(start: *const libc::c_void, len: libc::size_t);
    }

    unsafe { sys_icache_invalidate(ptr.cast(), len) };
}

#[cfg(all(unix, not(all(target_arch = "aarch64", target_vendor = "apple"))))]
unsafe fn flush_instruction_cache(_ptr: *mut u8, _len: usize) {}

impl Drop for ExecutableBuffer {
    fn drop(&mut self) {
        #[cfg(unix)]
        unsafe {
            libc::munmap(self.base.as_ptr().cast(), self.map_len);
        }
    }
}

/// A compiled function ready to execute.
pub struct CompiledFunction {
    /// The raw function pointer: `extern "C" fn(*mut JitContext) -> u64`
    pub entry: *const u8,
    /// Code size in bytes.
    pub code_size: usize,
    /// Which backend produced this machine code.
    pub origin: CompiledCodeOrigin,
    /// The owner that keeps the machine code alive.
    _owner: CompiledCodeOwner,
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

/// Install a raw code buffer as executable machine code.
pub fn compile_code_buffer(
    code: &CodeBuffer,
    origin: CompiledCodeOrigin,
) -> Result<CompiledFunction, JitError> {
    let executable = ExecutableBuffer::from_code_buffer(code)?;
    let entry = executable.entry();
    let code_size = code.len();

    Ok(CompiledFunction {
        entry,
        code_size,
        origin,
        _owner: CompiledCodeOwner::Executable {
            _buffer: executable,
        },
    })
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

    let cfg = crate::config::jit_config();

    // Dump CLIF IR before compilation if requested.
    if cfg.dump_clif {
        eprintln!("[JIT] === CLIF IR ===");
        eprintln!("{}", clif_func.display());
    }

    // Compile and define.
    let mut ctx = ClifContext::for_function(clif_func);
    module
        .define_function(func_id, &mut ctx)
        .map_err(|e| JitError::Cranelift(e.to_string()))?;

    // Dump native code if requested.
    if cfg.dump_asm && let Some(compiled) = ctx.compiled_code() {
        if let Some(vcode) = compiled.vcode.as_ref() {
            eprintln!("[JIT] === VCode (near-asm) ===");
            eprintln!("{vcode}");
        }
        let code = compiled.code_buffer();
        // Use real disassembler (iced-x86 on x86-64, hex on other arches).
        crate::codegen::disasm::dump_disassembly(code, 0, None);
    }

    // Finalize — make the code executable.
    module
        .finalize_definitions()
        .map_err(|e| JitError::Cranelift(e.to_string()))?;

    let code_ptr = module.get_finalized_function(func_id);
    let code_size = ctx.compiled_code().unwrap().code_info().total_size as usize;

    if cfg.dump_asm {
        eprintln!("[JIT] compiled function at {:p}, {} bytes", code_ptr, code_size);
        // Disassemble the finalized (relocated) code for accurate branch targets.
        let finalized_code =
            unsafe { std::slice::from_raw_parts(code_ptr, code_size) };
        crate::codegen::disasm::dump_disassembly(
            finalized_code,
            code_ptr as u64,
            Some("jit_entry (finalized)"),
        );
    }

    Ok(CompiledFunction {
        entry: code_ptr,
        code_size,
        origin: CompiledCodeOrigin::MirBaseline,
        _owner: CompiledCodeOwner::Cranelift {
            _module: Box::new(module),
        },
    })
}
