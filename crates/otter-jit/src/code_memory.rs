//! Executable memory management for JIT-compiled code.
//!
//! Owns the machine-code buffer produced by the template baseline
//! emitter, maps it into an executable page with `mmap` + `mprotect`,
//! and exposes a typed function pointer via [`CompiledFunction`]. The
//! buffer is released (via `munmap`) when the `CompiledFunction` is
//! dropped, so compiled code never outlives its wrapper.

use std::ptr::NonNull;

use crate::JitError;
use crate::arch::CodeBuffer;
use crate::context::JitContext;

/// Actual code generation backend that produced a compiled function.
///
/// Only one variant is currently produced — the template baseline — but
/// the enum is retained so future backends can be distinguished in
/// telemetry without plumbing a new type through the cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompiledCodeOrigin {
    /// Direct template-baseline `bytecode -> asm stencil` path.
    TemplateBaseline,
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
        // SAFETY: Allocating an anonymous private mapping with PROT_READ |
        // PROT_WRITE. All arguments are valid; we check the return value
        // against MAP_FAILED and convert null into an explicit error.
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

        // SAFETY: `base` is a freshly allocated, writable page-aligned
        // buffer at least `buf.len()` bytes long, and `buf.bytes()` is a
        // valid slice of that length. After the copy we flush I-cache
        // (aarch64-apple requirement) before flipping the page to
        // read+execute. If `mprotect` fails we `munmap` the page
        // immediately to avoid leaking address space.
        unsafe {
            std::ptr::copy_nonoverlapping(buf.bytes().as_ptr(), base.as_ptr(), buf.len());
            flush_instruction_cache(base.as_ptr(), buf.len());
            if libc::mprotect(
                base.as_ptr().cast(),
                map_len,
                libc::PROT_READ | libc::PROT_EXEC,
            ) != 0
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
    // SAFETY: `sysconf(_SC_PAGESIZE)` is a pure query; no memory safety
    // concerns. We validate the return value is positive before using it.
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

    // SAFETY: `sys_icache_invalidate` is a Darwin syscall; `ptr..ptr+len`
    // is the freshly copied code buffer, contiguous and valid for reads.
    unsafe { sys_icache_invalidate(ptr.cast(), len) };
}

#[cfg(all(unix, not(all(target_arch = "aarch64", target_vendor = "apple"))))]
unsafe fn flush_instruction_cache(_ptr: *mut u8, _len: usize) {}

impl Drop for ExecutableBuffer {
    fn drop(&mut self) {
        #[cfg(unix)]
        // SAFETY: We uniquely own the mapping; no other code can observe
        // the address after Drop.
        unsafe {
            libc::munmap(self.base.as_ptr().cast(), self.map_len);
        }
    }
}

/// A compiled function ready to execute.
///
/// Callable via [`Self::call`] as
/// `extern "C" fn(*mut JitContext) -> u64`. The owned executable buffer
/// is freed when this struct is dropped, so the function pointer must
/// not be invoked after that point.
pub struct CompiledFunction {
    /// The raw function pointer: `extern "C" fn(*mut JitContext) -> u64`.
    pub entry: *const u8,
    /// Code size in bytes.
    pub code_size: usize,
    /// Which backend produced this machine code.
    pub origin: CompiledCodeOrigin,
    _owner: ExecutableBuffer,
}

// SAFETY: The compiled code is immutable after finalization; the
// underlying mapping is owned by a single `ExecutableBuffer` which
// releases it only on Drop of the `CompiledFunction` that owns it.
unsafe impl Send for CompiledFunction {}
unsafe impl Sync for CompiledFunction {}

impl CompiledFunction {
    /// Machine-code size in bytes.
    #[must_use]
    pub fn size(&self) -> usize {
        self.code_size
    }

    /// Invoke the compiled function with `ctx` as its sole argument and
    /// return the NaN-boxed `u64` it produced.
    ///
    /// # Safety
    /// The caller must ensure:
    /// - `ctx` is a valid, fully initialized [`JitContext`].
    /// - The compiled code was produced for the function this context
    ///   describes (same bytecode, same register layout).
    /// - The host architecture matches the code buffer's target arch
    ///   (aarch64 today).
    pub unsafe fn call(&self, ctx: &mut JitContext) -> u64 {
        // SAFETY: `entry` was produced by our own pipeline; it has the
        // `extern "C" fn(*mut JitContext) -> u64` ABI by construction.
        // The caller guarantees `ctx` validity above.
        let func: unsafe extern "C" fn(*mut JitContext) -> u64 =
            unsafe { std::mem::transmute(self.entry) };
        unsafe { func(ctx) }
    }
}

/// Install a raw code buffer as executable machine code.
///
/// On non-unix hosts returns `JitError::Internal`; this function is the
/// single choke-point where the host-arch fallback policy lives, so the
/// rest of the pipeline does not need platform `cfg` gates.
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
        _owner: executable,
    })
}
