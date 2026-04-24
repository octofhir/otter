//! Executable memory management for JIT-compiled code.
//!
//! Owns the machine-code buffer produced by the template baseline
//! emitter, maps it into an executable page, and exposes a typed
//! function pointer via [`CompiledFunction`]. Unix hosts use `mmap`;
//! macOS ARM64 uses `MAP_JIT` plus `pthread_jit_write_protect_np` for
//! hardened-runtime W^X. The buffer is released (via `munmap`) when the
//! `CompiledFunction` is dropped, so compiled code never outlives its
//! wrapper.

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
        // SAFETY: Allocating an anonymous private mapping. On macOS
        // ARM64 this includes MAP_JIT and RWX page permissions, with
        // per-thread write protection toggled below. Other Unix hosts
        // start writable and are flipped to read+execute after copy.
        // All arguments are valid; we check the return value against
        // MAP_FAILED and convert null into an explicit error.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                map_len,
                mmap_protection(),
                mmap_flags(),
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
        // valid slice of that length. On macOS ARM64, `JitWriteScope`
        // disables the thread-local write-protect bit only while copying
        // and restores executable mode on drop. After the copy we flush
        // I-cache before finalizing the page protections. If finalization
        // fails we `munmap` the page immediately to avoid leaking address
        // space.
        unsafe {
            let _write_scope = JitWriteScope::enter();
            std::ptr::copy_nonoverlapping(buf.bytes().as_ptr(), base.as_ptr(), buf.len());
            flush_instruction_cache(base.as_ptr(), buf.len());
            if let Err(op) = finalize_executable(base.as_ptr(), map_len) {
                let err = std::io::Error::last_os_error();
                libc::munmap(base.as_ptr().cast(), map_len);
                return Err(JitError::Internal(format!("{op} failed: {err}")));
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
fn mmap_flags() -> libc::c_int {
    libc::MAP_PRIVATE | libc::MAP_ANON | platform_jit_map_flag()
}

#[cfg(all(unix, target_os = "macos", target_arch = "aarch64"))]
fn platform_jit_map_flag() -> libc::c_int {
    libc::MAP_JIT
}

#[cfg(all(unix, not(all(target_os = "macos", target_arch = "aarch64"))))]
fn platform_jit_map_flag() -> libc::c_int {
    0
}

#[cfg(all(unix, target_os = "macos", target_arch = "aarch64"))]
fn mmap_protection() -> libc::c_int {
    libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC
}

#[cfg(all(unix, not(all(target_os = "macos", target_arch = "aarch64"))))]
fn mmap_protection() -> libc::c_int {
    libc::PROT_READ | libc::PROT_WRITE
}

#[cfg(all(unix, target_os = "macos", target_arch = "aarch64"))]
unsafe fn finalize_executable(_base: *mut u8, _map_len: usize) -> Result<(), &'static str> {
    Ok(())
}

#[cfg(all(unix, not(all(target_os = "macos", target_arch = "aarch64"))))]
unsafe fn finalize_executable(base: *mut u8, map_len: usize) -> Result<(), &'static str> {
    // SAFETY: `base..base+map_len` is the mapping created above and is
    // still uniquely owned here. We remove write permission before the
    // function pointer becomes observable.
    let rc = unsafe { libc::mprotect(base.cast(), map_len, libc::PROT_READ | libc::PROT_EXEC) };
    if rc == 0 {
        Ok(())
    } else {
        Err("mprotect executable buffer")
    }
}

#[cfg(all(unix, target_os = "macos", target_arch = "aarch64"))]
struct JitWriteScope;

#[cfg(all(unix, target_os = "macos", target_arch = "aarch64"))]
impl JitWriteScope {
    unsafe fn enter() -> Self {
        unsafe extern "C" {
            fn pthread_jit_write_protect_np(enabled: libc::c_int);
        }

        // SAFETY: Darwin's MAP_JIT contract requires temporarily
        // disabling this thread's JIT write-protect bit before writing to
        // a JIT mapping. The guard restores executable mode in Drop.
        unsafe { pthread_jit_write_protect_np(0) };
        Self
    }
}

#[cfg(all(unix, target_os = "macos", target_arch = "aarch64"))]
impl Drop for JitWriteScope {
    fn drop(&mut self) {
        unsafe extern "C" {
            fn pthread_jit_write_protect_np(enabled: libc::c_int);
        }

        // SAFETY: Re-enables Darwin's per-thread write-protect bit after
        // the code bytes have been copied and the instruction cache has
        // been invalidated.
        unsafe { pthread_jit_write_protect_np(1) };
    }
}

#[cfg(all(unix, not(all(target_os = "macos", target_arch = "aarch64"))))]
struct JitWriteScope;

#[cfg(all(unix, not(all(target_os = "macos", target_arch = "aarch64"))))]
impl JitWriteScope {
    unsafe fn enter() -> Self {
        Self
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
    /// Per-loop-header OSR entry offsets: `(byte_pc, native_offset)`,
    /// sorted by `byte_pc` so cache lookups can do an O(log N) probe.
    /// Each `native_offset` is the byte offset of an OSR trampoline
    /// inside the executable buffer; the trampoline pins the JIT
    /// registers, rehydrates the accumulator from
    /// [`crate::context::JitContext::accumulator_raw`], and unconditional-
    /// jumps into the loop header's body.
    pub osr_entries: Vec<(u32, u32)>,
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
        osr_entries: Vec::new(),
        _owner: executable,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one_byte_buffer() -> CodeBuffer {
        let mut code = CodeBuffer::new();
        code.emit(&[0x90]);
        code
    }

    #[test]
    fn s4_empty_code_buffer_is_rejected() {
        let code = CodeBuffer::new();
        let err = match compile_code_buffer(&code, CompiledCodeOrigin::TemplateBaseline) {
            Ok(_) => panic!("empty code buffer must not be mapped"),
            Err(err) => err,
        };
        assert!(format!("{err}").contains("empty code buffer"));
    }

    #[cfg(unix)]
    #[test]
    fn s4_non_empty_code_buffer_installs_executable_mapping() {
        let code = one_byte_buffer();
        let compiled = compile_code_buffer(&code, CompiledCodeOrigin::TemplateBaseline)
            .expect("install non-empty code buffer");

        assert_eq!(compiled.size(), 1);
        assert_eq!(compiled.origin, CompiledCodeOrigin::TemplateBaseline);
        assert!(!compiled.entry.is_null());
    }

    #[cfg(unix)]
    #[test]
    fn s4_platform_mapping_flags_match_jit_policy() {
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        {
            assert_ne!(mmap_flags() & libc::MAP_JIT, 0);
            assert_ne!(mmap_protection() & libc::PROT_EXEC, 0);
        }

        #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
        {
            assert_eq!(platform_jit_map_flag(), 0);
            assert_eq!(mmap_protection() & libc::PROT_EXEC, 0);
        }
    }
}
