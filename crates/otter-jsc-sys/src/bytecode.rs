//! Bytecode cache API for JSC
//!
//! Provides functions to generate bytecode from JavaScript source code.
//! Works with bun-webkit, returns stub errors on system JSC.

use super::JSContextRef;
use std::os::raw::c_char;

/// Result of bytecode generation
#[repr(C)]
#[derive(Clone, Debug)]
pub struct OtterBytecodeResult {
    pub success: bool,
    pub data: *const u8,
    pub size: usize,
    pub error_message: [u8; 256],
}

impl Default for OtterBytecodeResult {
    fn default() -> Self {
        Self {
            success: false,
            data: std::ptr::null(),
            size: 0,
            error_message: [0; 256],
        }
    }
}

impl OtterBytecodeResult {
    /// Get error message as string
    pub fn error(&self) -> String {
        let end = self
            .error_message
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(self.error_message.len());
        String::from_utf8_lossy(&self.error_message[..end]).into_owned()
    }
}

unsafe extern "C" {
    /// Generate bytecode for a program (script) source code
    pub fn otter_generate_program_bytecode(
        ctx: JSContextRef,
        source: *const c_char,
        source_len: usize,
        filename: *const c_char,
        filename_len: usize,
        out: *mut OtterBytecodeResult,
    ) -> bool;

    /// Generate bytecode for a program and write to a file
    pub fn otter_generate_program_bytecode_to_file(
        ctx: JSContextRef,
        source: *const c_char,
        source_len: usize,
        filename: *const c_char,
        filename_len: usize,
        output_path: *const c_char,
        output_path_len: usize,
        out: *mut OtterBytecodeResult,
    ) -> bool;

    /// Generate bytecode for an ES module and write to a file
    pub fn otter_generate_module_bytecode_to_file(
        ctx: JSContextRef,
        source: *const c_char,
        source_len: usize,
        filename: *const c_char,
        filename_len: usize,
        output_path: *const c_char,
        output_path_len: usize,
        out: *mut OtterBytecodeResult,
    ) -> bool;

    /// Evaluate a script using cached bytecode
    pub fn otter_evaluate_with_cache(
        ctx: JSContextRef,
        source: *const c_char,
        source_len: usize,
        filename: *const c_char,
        filename_len: usize,
        bytecode_path: *const c_char,
        bytecode_path_len: usize,
        out: *mut OtterBytecodeResult,
    ) -> bool;
}

/// Safe wrapper to generate program bytecode to a file
///
/// # Safety
/// The context must be valid.
pub fn generate_program_bytecode_to_file(
    ctx: JSContextRef,
    source: &str,
    filename: &str,
    output_path: &str,
) -> Result<usize, String> {
    let mut result = OtterBytecodeResult::default();

    let success = unsafe {
        otter_generate_program_bytecode_to_file(
            ctx,
            source.as_ptr() as *const c_char,
            source.len(),
            filename.as_ptr() as *const c_char,
            filename.len(),
            output_path.as_ptr() as *const c_char,
            output_path.len(),
            &mut result,
        )
    };

    if success && result.success {
        Ok(result.size)
    } else {
        Err(result.error())
    }
}

/// Safe wrapper to generate module bytecode to a file
///
/// # Safety
/// The context must be valid.
pub fn generate_module_bytecode_to_file(
    ctx: JSContextRef,
    source: &str,
    filename: &str,
    output_path: &str,
) -> Result<usize, String> {
    let mut result = OtterBytecodeResult::default();

    let success = unsafe {
        otter_generate_module_bytecode_to_file(
            ctx,
            source.as_ptr() as *const c_char,
            source.len(),
            filename.as_ptr() as *const c_char,
            filename.len(),
            output_path.as_ptr() as *const c_char,
            output_path.len(),
            &mut result,
        )
    };

    if success && result.success {
        Ok(result.size)
    } else {
        Err(result.error())
    }
}

/// Safe wrapper to evaluate script with bytecode cache
pub fn evaluate_with_cache(
    ctx: JSContextRef,
    source: &str,
    filename: &str,
    bytecode_path: &str,
) -> Result<(), String> {
    let mut result = OtterBytecodeResult::default();

    let success = unsafe {
        otter_evaluate_with_cache(
            ctx,
            source.as_ptr() as *const c_char,
            source.len(),
            filename.as_ptr() as *const c_char,
            filename.len(),
            bytecode_path.as_ptr() as *const c_char,
            bytecode_path.len(),
            &mut result,
        )
    };

    if success && result.success {
        Ok(())
    } else {
        Err(result.error())
    }
}
