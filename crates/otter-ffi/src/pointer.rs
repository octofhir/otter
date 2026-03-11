//! Direct memory read operations for FFI pointer access.

use crate::error::FfiError;

fn checked_addr(ptr: usize, offset: usize) -> Result<usize, FfiError> {
    let addr = ptr.checked_add(offset).ok_or(FfiError::NullPointer)?;
    if addr == 0 {
        return Err(FfiError::NullPointer);
    }
    Ok(addr)
}

/// Read a u8 at `ptr + offset`.
///
/// # Safety
/// Caller must ensure `ptr + offset` is valid and readable.
pub unsafe fn read_u8(ptr: usize, offset: usize) -> Result<u8, FfiError> {
    let addr = checked_addr(ptr, offset)?;
    Ok(unsafe { *(addr as *const u8) })
}

/// Read an i8 at `ptr + offset`.
pub unsafe fn read_i8(ptr: usize, offset: usize) -> Result<i8, FfiError> {
    let addr = checked_addr(ptr, offset)?;
    Ok(unsafe { *(addr as *const i8) })
}

/// Read a u16 at `ptr + offset`.
pub unsafe fn read_u16(ptr: usize, offset: usize) -> Result<u16, FfiError> {
    let addr = checked_addr(ptr, offset)?;
    Ok(unsafe { std::ptr::read_unaligned(addr as *const u16) })
}

/// Read an i16 at `ptr + offset`.
pub unsafe fn read_i16(ptr: usize, offset: usize) -> Result<i16, FfiError> {
    let addr = checked_addr(ptr, offset)?;
    Ok(unsafe { std::ptr::read_unaligned(addr as *const i16) })
}

/// Read a u32 at `ptr + offset`.
pub unsafe fn read_u32(ptr: usize, offset: usize) -> Result<u32, FfiError> {
    let addr = checked_addr(ptr, offset)?;
    Ok(unsafe { std::ptr::read_unaligned(addr as *const u32) })
}

/// Read an i32 at `ptr + offset`.
pub unsafe fn read_i32(ptr: usize, offset: usize) -> Result<i32, FfiError> {
    let addr = checked_addr(ptr, offset)?;
    Ok(unsafe { std::ptr::read_unaligned(addr as *const i32) })
}

/// Read a u64 at `ptr + offset`.
pub unsafe fn read_u64(ptr: usize, offset: usize) -> Result<u64, FfiError> {
    let addr = checked_addr(ptr, offset)?;
    Ok(unsafe { std::ptr::read_unaligned(addr as *const u64) })
}

/// Read an i64 at `ptr + offset`.
pub unsafe fn read_i64(ptr: usize, offset: usize) -> Result<i64, FfiError> {
    let addr = checked_addr(ptr, offset)?;
    Ok(unsafe { std::ptr::read_unaligned(addr as *const i64) })
}

/// Read an f32 at `ptr + offset`.
pub unsafe fn read_f32(ptr: usize, offset: usize) -> Result<f32, FfiError> {
    let addr = checked_addr(ptr, offset)?;
    Ok(unsafe { std::ptr::read_unaligned(addr as *const f32) })
}

/// Read an f64 at `ptr + offset`.
pub unsafe fn read_f64(ptr: usize, offset: usize) -> Result<f64, FfiError> {
    let addr = checked_addr(ptr, offset)?;
    Ok(unsafe { std::ptr::read_unaligned(addr as *const f64) })
}

/// Read a pointer-sized value at `ptr + offset`.
pub unsafe fn read_ptr(ptr: usize, offset: usize) -> Result<usize, FfiError> {
    let addr = checked_addr(ptr, offset)?;
    Ok(unsafe { std::ptr::read_unaligned(addr as *const usize) })
}

/// Read an isize at `ptr + offset`.
pub unsafe fn read_intptr(ptr: usize, offset: usize) -> Result<isize, FfiError> {
    let addr = checked_addr(ptr, offset)?;
    Ok(unsafe { std::ptr::read_unaligned(addr as *const isize) })
}

/// Read a null-terminated C string at `ptr + offset`.
///
/// Returns the string content (UTF-8 lossy conversion).
pub unsafe fn read_cstring(ptr: usize, offset: usize) -> Result<String, FfiError> {
    let addr = checked_addr(ptr, offset)?;
    let cstr = unsafe { std::ffi::CStr::from_ptr(addr as *const i8) };
    Ok(cstr.to_string_lossy().into_owned())
}

/// Get the platform-specific shared library file extension.
pub fn platform_suffix() -> &'static str {
    if cfg!(target_os = "macos") {
        "dylib"
    } else if cfg!(target_os = "windows") {
        "dll"
    } else {
        "so"
    }
}
