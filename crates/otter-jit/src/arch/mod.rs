//! Architecture-specific code generation.
//!
//! Provides a thin macro-assembler layer for emitting small native code
//! sequences that bypass Cranelift. Used for IC stubs, fast-path templates,
//! and patchable call sites where Cranelift's compilation overhead is excessive.
//!
//! ## Supported architectures
//!
//! - **x86-64**: `arch::x64` module
//! - **AArch64**: `arch::aarch64` module
//!
//! Each architecture module provides:
//! - `Assembler`: raw instruction emission
//! - NaN-boxing tag check sequences
//! - IC stub templates (shape check + slot load)
//! - Patchable branch/call sequences

#[cfg(target_arch = "x86_64")]
pub mod x64;

#[cfg(target_arch = "aarch64")]
pub mod aarch64;

/// Target-independent description of the host platform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostPlatform {
    /// Pointer size in bytes.
    pub pointer_size: u8,
    /// Number of general-purpose registers available for allocation.
    pub gp_register_count: u8,
    /// Number of floating-point registers.
    pub fp_register_count: u8,
    /// Whether the platform uses a link register for calls (AArch64) vs stack (x86-64).
    pub has_link_register: bool,
    /// Architecture name for diagnostics.
    pub name: &'static str,
}

impl HostPlatform {
    /// Get the current host platform.
    #[must_use]
    pub fn current() -> Self {
        #[cfg(target_arch = "x86_64")]
        {
            Self {
                pointer_size: 8,
                gp_register_count: 16,   // rax-r15
                fp_register_count: 16,   // xmm0-xmm15
                has_link_register: false, // Uses stack for return address.
                name: "x86-64",
            }
        }
        #[cfg(target_arch = "aarch64")]
        {
            Self {
                pointer_size: 8,
                gp_register_count: 31,   // x0-x30
                fp_register_count: 32,   // v0-v31
                has_link_register: true,  // x30 (LR)
                name: "aarch64",
            }
        }
        #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
        {
            Self {
                pointer_size: 8,
                gp_register_count: 16,
                fp_register_count: 16,
                has_link_register: false,
                name: "unknown",
            }
        }
    }
}

/// A buffer of emitted machine code bytes.
#[derive(Debug, Clone)]
pub struct CodeBuffer {
    bytes: Vec<u8>,
    /// Relocations that need patching after the buffer is placed in memory.
    relocations: Vec<Relocation>,
}

/// A relocation: a position in the code buffer that needs patching.
#[derive(Debug, Clone)]
pub struct Relocation {
    /// Offset in the code buffer where the relocation applies.
    pub offset: u32,
    /// Kind of relocation.
    pub kind: RelocKind,
    /// Target address or symbol (resolved at install time).
    pub target: u64,
}

/// Relocation kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelocKind {
    /// Absolute 64-bit address.
    Abs64,
    /// PC-relative 32-bit offset (x86-64 rip-relative, AArch64 ADR/ADRP).
    PcRel32,
    /// Branch target (x86-64: rel32, AArch64: 26-bit offset in B/BL).
    Branch,
}

impl CodeBuffer {
    /// Create an empty code buffer.
    #[must_use]
    pub fn new() -> Self {
        Self {
            bytes: Vec::with_capacity(256),
            relocations: Vec::new(),
        }
    }

    /// Emit raw bytes.
    pub fn emit(&mut self, bytes: &[u8]) {
        self.bytes.extend_from_slice(bytes);
    }

    /// Emit a single byte.
    pub fn emit_u8(&mut self, byte: u8) {
        self.bytes.push(byte);
    }

    /// Emit a little-endian u32.
    pub fn emit_u32_le(&mut self, val: u32) {
        self.bytes.extend_from_slice(&val.to_le_bytes());
    }

    /// Emit a little-endian u64.
    pub fn emit_u64_le(&mut self, val: u64) {
        self.bytes.extend_from_slice(&val.to_le_bytes());
    }

    /// Read a little-endian u32 from an existing offset.
    #[must_use]
    pub fn read_u32_le(&self, offset: u32) -> Option<u32> {
        let start = offset as usize;
        let end = start.checked_add(4)?;
        let slice = self.bytes.get(start..end)?;
        let bytes: [u8; 4] = slice.try_into().ok()?;
        Some(u32::from_le_bytes(bytes))
    }

    /// Patch a little-endian u32 at an existing offset.
    pub fn patch_u32_le(&mut self, offset: u32, val: u32) -> bool {
        let start = offset as usize;
        let end = match start.checked_add(4) {
            Some(end) => end,
            None => return false,
        };
        let Some(slice) = self.bytes.get_mut(start..end) else {
            return false;
        };
        slice.copy_from_slice(&val.to_le_bytes());
        true
    }

    /// Current position (next byte offset).
    #[must_use]
    pub fn position(&self) -> u32 {
        self.bytes.len() as u32
    }

    /// Add a relocation.
    pub fn add_relocation(&mut self, reloc: Relocation) {
        self.relocations.push(reloc);
    }

    /// Get the emitted bytes.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Get pending relocations.
    #[must_use]
    pub fn relocations(&self) -> &[Relocation] {
        &self.relocations
    }

    /// Total code size in bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Whether the buffer is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }
}

impl Default for CodeBuffer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_host_platform() {
        let p = HostPlatform::current();
        assert_eq!(p.pointer_size, 8);

        #[cfg(target_arch = "x86_64")]
        assert_eq!(p.name, "x86-64");

        #[cfg(target_arch = "aarch64")]
        assert_eq!(p.name, "aarch64");
    }

    #[test]
    fn test_code_buffer_emit() {
        let mut buf = CodeBuffer::new();
        buf.emit(&[0x55, 0x48, 0x89, 0xe5]);
        buf.emit_u8(0xc3);
        assert_eq!(buf.len(), 5);
        assert_eq!(buf.bytes(), &[0x55, 0x48, 0x89, 0xe5, 0xc3]);
    }
}
