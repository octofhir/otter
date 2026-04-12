//! AArch64 macro-assembler for IC stubs and fast-path templates.
//!
//! Emits raw AArch64 machine code into a `CodeBuffer`. Fixed-width 32-bit
//! instructions, little-endian.
//!
//! ## Register convention (for IC stubs, AAPCS64)
//!
//! | Register | Purpose |
//! |----------|---------|
//! | x0       | JitContext pointer (arg0) |
//! | x1       | Object/receiver (arg1) |
//! | x2       | Property name / value (arg2) |
//! | x0       | Return value |
//! | x8-x17   | Scratch (caller-saved) |
//! | x30 (LR) | Return address |

use super::CodeBuffer;

/// AArch64 general-purpose registers (64-bit).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
#[allow(dead_code)]
pub enum Reg {
    X0 = 0, X1 = 1, X2 = 2, X3 = 3,
    X4 = 4, X5 = 5, X6 = 6, X7 = 7,
    X8 = 8, X9 = 9, X10 = 10, X11 = 11,
    X12 = 12, X13 = 13, X14 = 14, X15 = 15,
    X16 = 16, X17 = 17, X18 = 18, X19 = 19,
    X20 = 20, X21 = 21, X22 = 22, X23 = 23,
    X24 = 24, X25 = 25, X26 = 26, X27 = 27,
    X28 = 28, X29 = 29, // FP
    X30 = 30,            // LR
    Xzr = 31,            // Zero register / SP (context-dependent)
}

impl Reg {
    fn encoding(self) -> u32 {
        self as u32
    }
}

/// AArch64 assembler for emitting into a CodeBuffer.
pub struct Assembler<'a> {
    buf: &'a mut CodeBuffer,
}

impl<'a> Assembler<'a> {
    pub fn new(buf: &'a mut CodeBuffer) -> Self {
        Self { buf }
    }

    /// Emit a 32-bit instruction (little-endian).
    fn emit_insn(&mut self, insn: u32) {
        self.buf.emit_u32_le(insn);
    }

    // ---- Basic instructions ----

    /// `ret` (return to LR)
    pub fn ret(&mut self) {
        // RET x30: 0xD65F03C0
        self.emit_insn(0xD65F03C0);
    }

    /// `nop`
    pub fn nop(&mut self) {
        // NOP: 0xD503201F
        self.emit_insn(0xD503201F);
    }

    /// `mov dst, src` (64-bit register move via ORR)
    /// `ORR Xd, XZR, Xm` = `MOV Xd, Xm`
    pub fn mov_rr(&mut self, dst: Reg, src: Reg) {
        let insn = 0xAA000000
            | (src.encoding() << 16)
            | (Reg::Xzr.encoding() << 5)
            | dst.encoding();
        self.emit_insn(insn);
    }

    /// `movz dst, imm16` (move 16-bit immediate, zero other bits)
    /// Shift = 0 (bits 0-15).
    pub fn movz(&mut self, dst: Reg, imm16: u16) {
        let insn = 0xD2800000 | ((imm16 as u32) << 5) | dst.encoding();
        self.emit_insn(insn);
    }

    /// `movk dst, imm16, lsl #shift` (move 16-bit immediate, keep other bits)
    /// Shift must be 0, 16, 32, or 48.
    pub fn movk(&mut self, dst: Reg, imm16: u16, shift: u8) {
        let hw = (shift / 16) as u32;
        let insn = 0xF2800000 | (hw << 21) | ((imm16 as u32) << 5) | dst.encoding();
        self.emit_insn(insn);
    }

    /// Load a full 64-bit immediate into a register using movz + movk.
    pub fn mov_imm64(&mut self, dst: Reg, imm: u64) {
        self.movz(dst, imm as u16);
        if imm > 0xFFFF {
            self.movk(dst, (imm >> 16) as u16, 16);
        }
        if imm > 0xFFFF_FFFF {
            self.movk(dst, (imm >> 32) as u16, 32);
        }
        if imm > 0xFFFF_FFFF_FFFF {
            self.movk(dst, (imm >> 48) as u16, 48);
        }
    }

    /// `cmp xn, xm` (sets flags, 64-bit)
    /// Encoded as `SUBS XZR, Xn, Xm`
    pub fn cmp_rr(&mut self, a: Reg, b: Reg) {
        let insn = 0xEB000000
            | (b.encoding() << 16)
            | (a.encoding() << 5)
            | Reg::Xzr.encoding(); // Rd = XZR (discard result)
        self.emit_insn(insn);
    }

    /// `and dst, src, imm` — NOT a simple encoding on AArch64 (bitmask immediate).
    /// For IC stubs, we load the mask into a scratch register and use `and dst, src, scratch`.
    pub fn and_rr(&mut self, dst: Reg, src: Reg, mask_reg: Reg) {
        // AND Xd, Xn, Xm
        let insn = 0x8A000000
            | (mask_reg.encoding() << 16)
            | (src.encoding() << 5)
            | dst.encoding();
        self.emit_insn(insn);
    }

    /// `cbz xn, offset` (compare and branch if zero)
    /// offset is in bytes, must be aligned to 4, range ±1MB.
    pub fn cbz(&mut self, reg: Reg, byte_offset: i32) {
        let imm19 = ((byte_offset >> 2) as u32) & 0x7FFFF;
        let insn = 0xB4000000 | (imm19 << 5) | reg.encoding();
        self.emit_insn(insn);
    }

    /// `cbnz xn, offset` (compare and branch if not zero)
    pub fn cbnz(&mut self, reg: Reg, byte_offset: i32) {
        let imm19 = ((byte_offset >> 2) as u32) & 0x7FFFF;
        let insn = 0xB5000000 | (imm19 << 5) | reg.encoding();
        self.emit_insn(insn);
    }

    // ---- NaN-boxing helpers ----

    /// Check if value in `src` has the Int32 tag.
    /// After this, branch with B.EQ (ZF set if Int32).
    /// Clobbers x8, x9.
    pub fn check_int32_tag(&mut self, src: Reg) {
        // x8 = src & INT32_TAG_MASK (upper 32 bits)
        self.mov_imm64(Reg::X9, 0xFFFF_FFFF_0000_0000);
        self.and_rr(Reg::X8, src, Reg::X9);
        // x9 = TAG_INT32
        self.mov_imm64(Reg::X9, 0x7FF8_0001_0000_0000);
        // cmp x8, x9 (ZF set if match)
        self.cmp_rr(Reg::X8, Reg::X9);
    }

    /// Check if value in `src` is an object pointer.
    /// Clobbers x8, x9.
    pub fn check_object_tag(&mut self, src: Reg) {
        self.mov_imm64(Reg::X9, 0xFFFF_0000_0000_0000);
        self.and_rr(Reg::X8, src, Reg::X9);
        self.mov_imm64(Reg::X9, 0x7FFC_0000_0000_0000);
        self.cmp_rr(Reg::X8, Reg::X9);
    }

    /// Extract i32 payload from NaN-boxed value.
    /// Lower 32 bits of `src` are the i32.
    pub fn extract_int32(&mut self, dst: Reg, src: Reg) {
        // UXTW: zero-extend 32-bit to 64-bit.
        // Encoded as UBFM Xd, Xn, #0, #31
        let insn = 0xD3407C00 | (src.encoding() << 5) | dst.encoding();
        self.emit_insn(insn);
    }

    /// Box an i32 into NaN-boxed format.
    /// `src` has i32 in lower 32 bits. Result in `dst`.
    pub fn box_int32(&mut self, dst: Reg, src: Reg) {
        // dst = TAG_INT32
        self.mov_imm64(dst, 0x7FF8_0001_0000_0000);
        // x9 = zero-extend src to 64-bit
        self.extract_int32(Reg::X9, src);
        // dst = dst | x9
        let insn = 0xAA000000
            | (Reg::X9.encoding() << 16)
            | (dst.encoding() << 5)
            | dst.encoding(); // ORR dst, dst, x9
        self.emit_insn(insn);
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
        assert_eq!(buf.len(), 4);
        assert_eq!(buf.bytes(), &[0xC0, 0x03, 0x5F, 0xD6]); // RET in LE
    }

    #[test]
    fn test_emit_nop() {
        let mut buf = CodeBuffer::new();
        let mut asm = Assembler::new(&mut buf);
        asm.nop();
        assert_eq!(buf.len(), 4);
        assert_eq!(buf.bytes(), &[0x1F, 0x20, 0x03, 0xD5]); // NOP in LE
    }

    #[test]
    fn test_emit_movz() {
        let mut buf = CodeBuffer::new();
        let mut asm = Assembler::new(&mut buf);
        asm.movz(Reg::X0, 42);
        assert_eq!(buf.len(), 4);
        // MOVZ X0, #42: 0xD2800540
        let insn = u32::from_le_bytes(buf.bytes().try_into().unwrap());
        assert_eq!(insn & 0xFF800000, 0xD2800000); // MOVZ opcode
        assert_eq!(insn & 0x1F, 0); // Rd = X0
    }

    #[test]
    fn test_mov_imm64_small() {
        let mut buf = CodeBuffer::new();
        let mut asm = Assembler::new(&mut buf);
        asm.mov_imm64(Reg::X0, 42);
        // Small value: just movz (4 bytes).
        assert_eq!(buf.len(), 4);
    }

    #[test]
    fn test_mov_imm64_large() {
        let mut buf = CodeBuffer::new();
        let mut asm = Assembler::new(&mut buf);
        asm.mov_imm64(Reg::X0, 0x7FF8_0001_0000_0000);
        // Large value: movz + 3x movk = 16 bytes.
        assert_eq!(buf.len(), 16);
    }

    #[test]
    fn test_check_int32_tag_emits() {
        let mut buf = CodeBuffer::new();
        let mut asm = Assembler::new(&mut buf);
        asm.check_int32_tag(Reg::X1);
        // Should emit movimm64 + and + movimm64 + cmp = multiple instructions.
        assert!(buf.len() >= 20, "tag check should emit substantial code, got {} bytes", buf.len());
    }
}
