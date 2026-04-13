//! x86-64 macro-assembler for IC stubs and fast-path templates.
//!
//! Emits raw x86-64 machine code into a `CodeBuffer`. Used for IC stubs
//! where Cranelift's compilation overhead would be excessive.
//!
//! ## Register convention (for IC stubs)
//!
//! | Register | Purpose |
//! |----------|---------|
//! | rdi      | JitContext pointer (arg0, System V ABI) |
//! | rsi      | Object/receiver (arg1) |
//! | rdx      | Property name / value (arg2) |
//! | rax      | Return value / scratch |
//! | rcx      | Scratch |
//! | r8-r11   | Scratch |

use super::CodeBuffer;

/// x86-64 general-purpose registers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
#[allow(dead_code)]
pub enum Reg {
    Rax = 0,
    Rcx = 1,
    Rdx = 2,
    Rbx = 3,
    Rsp = 4,
    Rbp = 5,
    Rsi = 6,
    Rdi = 7,
    R8 = 8,
    R9 = 9,
    R10 = 10,
    R11 = 11,
    R12 = 12,
    R13 = 13,
    R14 = 14,
    R15 = 15,
}

impl Reg {
    fn encoding(self) -> u8 {
        self as u8
    }
    fn is_extended(self) -> bool {
        (self as u8) >= 8
    }
    fn low3(self) -> u8 {
        self.encoding() & 0x7
    }
}

/// x86-64 assembler for emitting into a CodeBuffer.
pub struct Assembler<'a> {
    buf: &'a mut CodeBuffer,
}

impl<'a> Assembler<'a> {
    /// Create an assembler writing to the given buffer.
    pub fn new(buf: &'a mut CodeBuffer) -> Self {
        Self { buf }
    }

    // ---- REX prefix ----

    fn rex(&mut self, w: bool, r: Reg, b: Reg) {
        let mut rex = 0x40;
        if w {
            rex |= 0x08;
        }
        if r.is_extended() {
            rex |= 0x04;
        }
        if b.is_extended() {
            rex |= 0x01;
        }
        if rex != 0x40 {
            self.buf.emit_u8(rex);
        }
    }

    fn rex_w(&mut self, r: Reg, b: Reg) {
        self.rex(true, r, b);
    }

    fn modrm_rr(&mut self, reg: Reg, rm: Reg) {
        self.buf.emit_u8(0xC0 | (reg.low3() << 3) | rm.low3());
    }

    // ---- Basic instructions ----

    /// `mov reg, imm64`
    pub fn mov_imm64(&mut self, dst: Reg, imm: u64) {
        self.rex_w(Reg::Rax, dst);
        self.buf.emit_u8(0xB8 + dst.low3());
        self.buf.emit_u64_le(imm);
    }

    /// `mov dst, src` (64-bit register-to-register)
    pub fn mov_rr(&mut self, dst: Reg, src: Reg) {
        self.rex_w(src, dst);
        self.buf.emit_u8(0x89);
        self.modrm_rr(src, dst);
    }

    /// `cmp reg, imm32` (sign-extended)
    pub fn cmp_imm32(&mut self, reg: Reg, imm: i32) {
        self.rex_w(Reg::Rax, reg);
        if reg == Reg::Rax {
            self.buf.emit_u8(0x3D);
        } else {
            self.buf.emit_u8(0x81);
            self.modrm_rr(Reg::Rdi /* /7 */, reg);
        }
        self.buf.emit_u32_le(imm as u32);
    }

    /// `test reg, reg` (set flags without storing result)
    pub fn test_rr(&mut self, a: Reg, b: Reg) {
        self.rex_w(a, b);
        self.buf.emit_u8(0x85);
        self.modrm_rr(a, b);
    }

    /// `and dst, imm32` (64-bit)
    pub fn and_imm32(&mut self, dst: Reg, imm: i32) {
        self.rex_w(Reg::Rax, dst);
        if dst == Reg::Rax {
            self.buf.emit_u8(0x25);
        } else {
            self.buf.emit_u8(0x81);
            self.modrm_rr(Reg::Rsp /* /4 */, dst);
        }
        self.buf.emit_u32_le(imm as u32);
    }

    /// `ret`
    pub fn ret(&mut self) {
        self.buf.emit_u8(0xC3);
    }

    /// `push reg`
    pub fn push(&mut self, reg: Reg) {
        if reg.is_extended() {
            self.buf.emit_u8(0x41);
        }
        self.buf.emit_u8(0x50 + reg.low3());
    }

    /// `pop reg`
    pub fn pop(&mut self, reg: Reg) {
        if reg.is_extended() {
            self.buf.emit_u8(0x41);
        }
        self.buf.emit_u8(0x58 + reg.low3());
    }

    /// `nop`
    pub fn nop(&mut self) {
        self.buf.emit_u8(0x90);
    }

    // ---- NaN-boxing helpers ----

    /// Emit a tag check: test if value in `src` has the Int32 tag.
    /// Sets ZF if the value IS an Int32.
    ///
    /// ```text
    /// mov rax, src
    /// and rax, 0xFFFFFFFF00000000   ; INT32_TAG_MASK
    /// cmp rax, TAG_INT32            ; 0x7FF8000100000000
    /// ; ZF set if match → je target
    /// ```
    pub fn check_int32_tag(&mut self, src: Reg) {
        use otter_vm::value::{INT32_TAG_MASK, TAG_INT32};
        // mov rax, src
        self.mov_rr(Reg::Rax, src);
        // movabs rcx, INT32_TAG_MASK
        self.mov_imm64(Reg::Rcx, INT32_TAG_MASK);
        // and rax, rcx
        self.rex_w(Reg::Rax, Reg::Rax);
        self.buf.emit_u8(0x21); // and r/m64, r64
        self.modrm_rr(Reg::Rcx, Reg::Rax);
        // movabs rcx, TAG_INT32
        self.mov_imm64(Reg::Rcx, TAG_INT32);
        // cmp rax, rcx
        self.rex_w(Reg::Rax, Reg::Rcx);
        self.buf.emit_u8(0x39); // cmp r/m64, r64
        self.modrm_rr(Reg::Rcx, Reg::Rax);
        // ZF set if match
    }

    /// Emit a tag check: test if value in `src` is an object pointer.
    /// Sets ZF if the value IS an object.
    pub fn check_object_tag(&mut self, src: Reg) {
        use otter_vm::value::{OBJECT_TAG_MASK, TAG_PTR_OBJECT};
        self.mov_rr(Reg::Rax, src);
        self.mov_imm64(Reg::Rcx, OBJECT_TAG_MASK); // TAG_MASK
        self.rex_w(Reg::Rax, Reg::Rax);
        self.buf.emit_u8(0x21);
        self.modrm_rr(Reg::Rcx, Reg::Rax);
        self.mov_imm64(Reg::Rcx, TAG_PTR_OBJECT); // TAG_PTR_OBJECT
        self.rex_w(Reg::Rax, Reg::Rcx);
        self.buf.emit_u8(0x39);
        self.modrm_rr(Reg::Rcx, Reg::Rax);
    }

    /// Extract the i32 payload from a NaN-boxed Int32 value.
    /// Assumes `src` has already been verified as Int32.
    /// Result in lower 32 bits of `dst`.
    pub fn extract_int32(&mut self, dst: Reg, src: Reg) {
        // mov dst, src (full 64 bits)
        self.mov_rr(dst, src);
        // The lower 32 bits are the int32 payload — no further masking needed.
        // (Upper bits are the tag, which callers ignore after unboxing.)
    }

    /// Box an i32 value into NaN-boxed format.
    /// `src` has the i32 in lower 32 bits.
    /// Result: TAG_INT32 | (src as u32)
    pub fn box_int32(&mut self, dst: Reg, src: Reg) {
        use otter_vm::value::TAG_INT32;
        // movabs dst, TAG_INT32
        self.mov_imm64(dst, TAG_INT32);

        // Zero-extend src to 64-bit in rcx.
        // mov ecx, src (32-bit mov zero-extends to 64-bit)
        if src.is_extended() {
            self.buf.emit_u8(0x44);
        }
        self.buf.emit_u8(0x89);
        self.modrm_rr(src, Reg::Rcx);
        // or dst, rcx
        self.rex_w(Reg::Rcx, dst);
        self.buf.emit_u8(0x09);
        self.modrm_rr(Reg::Rcx, dst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_emit_ret() {
        let mut buf = CodeBuffer::new();
        let mut asm = Assembler::new(&mut buf);
        asm.ret();
        assert_eq!(buf.bytes(), &[0xC3]);
    }

    #[test]
    fn test_emit_push_pop() {
        let mut buf = CodeBuffer::new();
        let mut asm = Assembler::new(&mut buf);
        asm.push(Reg::Rbp);
        asm.pop(Reg::Rbp);
        assert_eq!(buf.bytes(), &[0x55, 0x5D]);
    }

    #[test]
    fn test_emit_push_extended() {
        let mut buf = CodeBuffer::new();
        let mut asm = Assembler::new(&mut buf);
        asm.push(Reg::R12);
        // REX.B prefix (0x41) + push r12 (0x54)
        assert_eq!(buf.bytes(), &[0x41, 0x54]);
    }

    #[test]
    fn test_emit_mov_imm64() {
        let mut buf = CodeBuffer::new();
        let mut asm = Assembler::new(&mut buf);
        asm.mov_imm64(Reg::Rax, 0xDEADBEEF_CAFEBABE);
        // REX.W (0x48) + mov rax, imm64 (0xB8) + 8 bytes
        assert_eq!(buf.len(), 10);
        assert_eq!(buf.bytes()[0], 0x48); // REX.W
        assert_eq!(buf.bytes()[1], 0xB8); // mov rax, imm64
    }

    #[test]
    fn test_emit_nop() {
        let mut buf = CodeBuffer::new();
        let mut asm = Assembler::new(&mut buf);
        asm.nop();
        assert_eq!(buf.bytes(), &[0x90]);
    }

    #[test]
    fn test_check_int32_tag_emits_code() {
        let mut buf = CodeBuffer::new();
        let mut asm = Assembler::new(&mut buf);
        asm.check_int32_tag(Reg::Rsi);
        // Should emit a non-trivial sequence.
        assert!(
            buf.len() > 10,
            "tag check should emit multiple instructions"
        );
    }
}
