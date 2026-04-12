//! Native code disassembly for JIT-compiled functions.
//!
//! Provides human-readable disassembly output for `OTTER_JIT_DUMP_ASM=1`.
//!
//! - **x86-64**: `iced-x86` (pure Rust, Intel syntax)
//! - **AArch64**: `bad64` (pure Rust, Binary Ninja's decoder)
//! - **Other**: hex dump fallback
//!
//! ## Output format
//!
//! ```text
//! [JIT] === Disassembly: function_name (123 bytes) ===
//!   0000: 55                   push    rbp
//!   0001: 48 89 e5             mov     rbp, rsp
//!   ...
//! ```

use std::fmt::Write;

/// Disassemble native code bytes into a human-readable string.
///
/// - `code`: raw machine code bytes
/// - `base_address`: virtual address of the first byte (for branch targets)
/// - `function_name`: optional name for the header
pub fn disassemble(code: &[u8], base_address: u64, function_name: Option<&str>) -> String {
    let mut output = String::with_capacity(code.len() * 8);

    let name = function_name.unwrap_or("<anonymous>");
    let _ = writeln!(
        output,
        "[JIT] === Disassembly: {} ({} bytes) ===",
        name,
        code.len()
    );

    disassemble_impl(code, base_address, &mut output);
    output
}

/// Print disassembly to stderr (convenience for dump_asm flag).
pub fn dump_disassembly(code: &[u8], base_address: u64, function_name: Option<&str>) {
    let text = disassemble(code, base_address, function_name);
    eprint!("{text}");
}

// ============================================================
// x86-64 implementation via iced-x86
// ============================================================

#[cfg(target_arch = "x86_64")]
fn disassemble_impl(code: &[u8], base_address: u64, output: &mut String) {
    use iced_x86::{Decoder, DecoderOptions, Formatter, IntelFormatter};

    let mut decoder = Decoder::with_ip(64, code, base_address, DecoderOptions::NONE);
    let mut formatter = IntelFormatter::new();

    // Show RIP-relative addresses as absolute for easier reading.
    formatter.options_mut().set_rip_relative_addresses(false);
    // Don't pad branch targets with leading zeros.
    formatter.options_mut().set_branch_leading_zeros(false);
    formatter.options_mut().set_uppercase_hex(false);

    let mut instruction = iced_x86::Instruction::default();
    let mut formatted = String::new();

    while decoder.can_decode() {
        let offset = decoder.position();
        decoder.decode_out(&mut instruction);

        // Format the hex bytes.
        let instr_bytes = &code[offset..offset + instruction.len()];
        let hex: String = instr_bytes.iter().map(|b| format!("{b:02x} ")).collect();

        // Format the mnemonic + operands.
        formatted.clear();
        formatter.format(&instruction, &mut formatted);

        let _ = writeln!(output, "  {offset:04x}: {hex:<24} {formatted}");
    }
}

// ============================================================
// AArch64 implementation via bad64
// ============================================================

#[cfg(target_arch = "aarch64")]
fn disassemble_impl(code: &[u8], base_address: u64, output: &mut String) {
    // AArch64 instructions are always 4 bytes, little-endian.
    if !code.len().is_multiple_of(4) {
        let _ = writeln!(
            output,
            "  (warning: code size {} is not a multiple of 4)",
            code.len()
        );
    }

    for (i, chunk) in code.chunks(4).enumerate() {
        let offset = i * 4;
        let addr = base_address + offset as u64;

        // Format the hex bytes.
        let hex: String = chunk.iter().map(|b| format!("{b:02x} ")).collect();

        // Decode the instruction.
        if chunk.len() == 4 {
            let word = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
            match bad64::decode(word, addr) {
                Ok(insn) => {
                    let _ = writeln!(output, "  {offset:04x}: {hex:<16} {insn}");
                }
                Err(_) => {
                    let _ = writeln!(output, "  {offset:04x}: {hex:<16} .word 0x{word:08x}");
                }
            }
        } else {
            let _ = writeln!(output, "  {offset:04x}: {hex:<16} (truncated)");
        }
    }
}

// ============================================================
// Fallback: hex dump for other architectures
// ============================================================

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
fn disassemble_impl(code: &[u8], _base_address: u64, output: &mut String) {
    let _ = writeln!(
        output,
        "  (native disassembly not available on this architecture; hex dump follows)"
    );
    for (i, chunk) in code.chunks(16).enumerate() {
        let offset = i * 16;
        let hex: String = chunk.iter().map(|b| format!("{b:02x} ")).collect();
        let _ = writeln!(output, "  {offset:04x}: {hex}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_disassemble_basic() {
        #[cfg(target_arch = "x86_64")]
        {
            // push rbp; mov rbp, rsp; pop rbp; ret
            let code = [0x55, 0x48, 0x89, 0xe5, 0x5d, 0xc3];
            let result = disassemble(&code, 0, Some("test_fn"));
            assert!(result.contains("push"));
            assert!(result.contains("rbp"));
            assert!(result.contains("ret"));
            assert!(result.contains("test_fn"));
            assert!(result.contains("6 bytes"));
        }

        #[cfg(target_arch = "aarch64")]
        {
            // ret (0xd65f03c0)
            let code = [0xc0, 0x03, 0x5f, 0xd6];
            let result = disassemble(&code, 0, Some("test_fn"));
            assert!(result.contains("ret"));
            assert!(result.contains("test_fn"));
            assert!(result.contains("4 bytes"));
        }
    }

    #[test]
    fn test_disassemble_empty() {
        let result = disassemble(&[], 0, None);
        assert!(result.contains("<anonymous>"));
        assert!(result.contains("0 bytes"));
    }

    #[test]
    fn test_disassemble_with_base_address() {
        // Verify non-zero base address doesn't panic.
        let code = [0xc3]; // ret on x86-64
        let result = disassemble(&code, 0x1000, Some("relocated"));
        assert!(result.contains("relocated"));
    }
}
