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

/// Condition code for `B.cond`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Cond {
    Eq = 0x0,
    Ne = 0x1,
    Ge = 0xA,
    Lt = 0xB,
    Gt = 0xC,
    Le = 0xD,
}

/// AArch64 general-purpose registers (64-bit).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Reg {
    X0 = 0,
    X1 = 1,
    X2 = 2,
    X3 = 3,
    X4 = 4,
    X5 = 5,
    X6 = 6,
    X7 = 7,
    X8 = 8,
    X9 = 9,
    X10 = 10,
    X11 = 11,
    X12 = 12,
    X13 = 13,
    X14 = 14,
    X15 = 15,
    X16 = 16,
    X17 = 17,
    X18 = 18,
    X19 = 19,
    X20 = 20,
    X21 = 21,
    X22 = 22,
    X23 = 23,
    X24 = 24,
    X25 = 25,
    X26 = 26,
    X27 = 27,
    X28 = 28,
    X29 = 29, // FP
    X30 = 30, // LR
    Xzr = 31, // Zero register / SP (context-dependent)
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

    /// Current byte position in the underlying code buffer.
    #[must_use]
    pub fn position(&self) -> u32 {
        self.buf.position()
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

    /// `blr xn` (branch with link to register)
    pub fn blr(&mut self, reg: Reg) {
        let insn = 0xD63F0000 | (reg.encoding() << 5);
        self.emit_insn(insn);
    }

    /// `br xn` (branch to register)
    pub fn br(&mut self, reg: Reg) {
        let insn = 0xD61F0000 | (reg.encoding() << 5);
        self.emit_insn(insn);
    }

    /// Push x19 and x30 (LR) to the stack.
    /// `sub sp, sp, #16; str x19, [sp]; str x30, [sp, #8]`
    pub fn push_x19_lr(&mut self) {
        self.emit_insn(0xD10043FF); // sub sp, sp, #16
        self.emit_insn(0xF90003F3); // str x19, [sp]
        self.emit_insn(0xF90007FE); // str x30, [sp, #8]
    }

    /// Pop x19 and x30 (LR) from the stack.
    /// `ldr x19, [sp]; ldr x30, [sp, #8]; add sp, sp, #16`
    pub fn pop_x19_lr(&mut self) {
        self.emit_insn(0xF94003F3); // ldr x19, [sp]
        self.emit_insn(0xF94007FE); // ldr x30, [sp, #8]
        self.emit_insn(0x910043FF); // add sp, sp, #16
    }

    /// `nop`
    pub fn nop(&mut self) {
        // NOP: 0xD503201F
        self.emit_insn(0xD503201F);
    }

    /// `mov dst, src` (64-bit register move via ORR)
    /// `ORR Xd, XZR, Xm` = `MOV Xd, Xm`
    pub fn mov_rr(&mut self, dst: Reg, src: Reg) {
        let insn =
            0xAA000000 | (src.encoding() << 16) | (Reg::Xzr.encoding() << 5) | dst.encoding();
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
        let insn = 0xEB000000 | (b.encoding() << 16) | (a.encoding() << 5) | Reg::Xzr.encoding(); // Rd = XZR (discard result)
        self.emit_insn(insn);
    }

    /// `add xd, xn, xm`
    pub fn add_rrr(&mut self, dst: Reg, lhs: Reg, rhs: Reg) {
        let insn = 0x8B000000 | (rhs.encoding() << 16) | (lhs.encoding() << 5) | dst.encoding();
        self.emit_insn(insn);
    }

    /// `add xd, xn, xm, lsl #shift`
    pub fn add_rrr_lsl(&mut self, dst: Reg, lhs: Reg, rhs: Reg, shift: u8) {
        let insn = 0x8B000000
            | ((shift as u32) << 10)
            | (rhs.encoding() << 16)
            | (lhs.encoding() << 5)
            | dst.encoding();
        self.emit_insn(insn);
    }

    /// `sub xd, xn, xm`
    pub fn sub_rrr(&mut self, dst: Reg, lhs: Reg, rhs: Reg) {
        let insn = 0xCB000000 | (rhs.encoding() << 16) | (lhs.encoding() << 5) | dst.encoding();
        self.emit_insn(insn);
    }

    /// `mul xd, xn, xm` via `MADD xd, xn, xm, xzr`
    pub fn mul_rrr(&mut self, dst: Reg, lhs: Reg, rhs: Reg) {
        let insn = 0x9B000000
            | (rhs.encoding() << 16)
            | (Reg::Xzr.encoding() << 10)
            | (lhs.encoding() << 5)
            | dst.encoding();
        self.emit_insn(insn);
    }

    /// `ldr xt, [xn, #imm]` for 64-bit loads using the unsigned immediate form.
    /// Offset must be non-negative, 8-byte aligned, and fit in 12 bits.
    pub fn ldr_u64_imm(&mut self, dst: Reg, base: Reg, byte_offset: u32) {
        debug_assert_eq!(byte_offset % 8, 0);
        let imm12 = byte_offset / 8;
        let insn = 0xF9400000 | (imm12 << 10) | (base.encoding() << 5) | dst.encoding();
        self.emit_insn(insn);
    }

    /// `str xt, [xn, #imm]` for 64-bit stores using the unsigned immediate form.
    /// Offset must be non-negative, 8-byte aligned, and fit in 12 bits.
    pub fn str_u64_imm(&mut self, src: Reg, base: Reg, byte_offset: u32) {
        debug_assert_eq!(byte_offset % 8, 0);
        let imm12 = byte_offset / 8;
        let insn = 0xF9000000 | (imm12 << 10) | (base.encoding() << 5) | src.encoding();
        self.emit_insn(insn);
    }

    /// `and dst, src, imm` — NOT a simple encoding on AArch64 (bitmask immediate).
    /// For IC stubs, we load the mask into a scratch register and use `and dst, src, scratch`.
    pub fn and_rr(&mut self, dst: Reg, src: Reg, mask_reg: Reg) {
        // AND Xd, Xn, Xm
        let insn =
            0x8A000000 | (mask_reg.encoding() << 16) | (src.encoding() << 5) | dst.encoding();
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

    /// Emit an unconditional branch placeholder (`b`) and return its offset.
    pub fn b_placeholder(&mut self) -> u32 {
        let pos = self.buf.position();
        self.emit_insn(0x14000000);
        pos
    }

    /// Emit a conditional branch placeholder (`b.cond`) and return its offset.
    pub fn b_cond_placeholder(&mut self, cond: Cond) -> u32 {
        let pos = self.buf.position();
        self.emit_insn(0x54000000 | (cond as u32));
        pos
    }

    // ---- NaN-boxing helpers ----

    /// Check if value in `src` has the Int32 tag.
    /// After this, branch with B.EQ (ZF set if Int32).
    /// Clobbers x8, x9.
    pub fn check_int32_tag(&mut self, src: Reg) {
        use otter_vm::value::{INT32_TAG_MASK, TAG_INT32};
        // x8 = src & INT32_TAG_MASK (upper 32 bits)
        self.mov_imm64(Reg::X9, INT32_TAG_MASK);
        self.and_rr(Reg::X8, src, Reg::X9);
        // x9 = TAG_INT32
        self.mov_imm64(Reg::X9, TAG_INT32);
        // cmp x8, x9 (ZF set if match)
        self.cmp_rr(Reg::X8, Reg::X9);
    }

    /// Check if value in `src` is an object pointer.
    /// Clobbers x8, x9.
    pub fn check_object_tag(&mut self, src: Reg) {
        use otter_vm::value::{OBJECT_TAG_MASK, TAG_PTR_OBJECT};
        self.mov_imm64(Reg::X9, OBJECT_TAG_MASK);
        self.and_rr(Reg::X8, src, Reg::X9);
        self.mov_imm64(Reg::X9, TAG_PTR_OBJECT);
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
        // Keep the extracted payload in a scratch register so callers can keep
        // long-lived pointers (for example registers_base in x9) alive.
        self.extract_int32(Reg::X12, src);
        // dst = TAG_INT32
        self.mov_imm64(dst, 0x7FF8_0001_0000_0000);
        // dst = dst | x12
        let insn =
            0xAA000000 | (Reg::X12.encoding() << 16) | (dst.encoding() << 5) | dst.encoding(); // ORR dst, dst, x12
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
        assert!(
            buf.len() >= 20,
            "tag check should emit substantial code, got {} bytes",
            buf.len()
        );
    }

    #[test]
    fn test_emit_load_store_and_branch_placeholders() {
        let mut buf = CodeBuffer::new();
        let mut asm = Assembler::new(&mut buf);
        asm.ldr_u64_imm(Reg::X9, Reg::X0, 0);
        asm.str_u64_imm(Reg::X10, Reg::X9, 16);
        let branch = asm.b_placeholder();
        let cond = asm.b_cond_placeholder(Cond::Ge);

        assert_eq!(branch, 8);
        assert_eq!(cond, 12);
        assert_eq!(buf.len(), 16);
    }
}
