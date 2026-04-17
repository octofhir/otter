//! x86-64 macro-assembler for the template baseline and small fast paths.
//!
//! Emits raw x86-64 machine code into a [`CodeBuffer`](super::CodeBuffer)
//! with a minimal instruction set tailored to the v2 baseline emitter.

use super::CodeBuffer;

/// x86-64 general-purpose registers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
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
        self.encoding() >= 8
    }

    fn low3(self) -> u8 {
        self.encoding() & 0x7
    }
}

/// x86-64 conditional branches used by the template baseline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Cond {
    Eq = 0x84,
    Ne = 0x85,
    Lt = 0x8C,
    Ge = 0x8D,
    Le = 0x8E,
    Gt = 0x8F,
}

/// x86-64 assembler for emitting into a [`CodeBuffer`].
pub struct Assembler<'a> {
    buf: &'a mut CodeBuffer,
}

impl<'a> Assembler<'a> {
    /// Create an assembler writing to `buf`.
    pub fn new(buf: &'a mut CodeBuffer) -> Self {
        Self { buf }
    }

    /// Current byte offset in the underlying code buffer.
    pub fn position(&self) -> u32 {
        self.buf.position()
    }

    fn rex_bits(&mut self, w: bool, r: bool, x: bool, b: bool) {
        let mut rex = 0x40;
        if w {
            rex |= 0x08;
        }
        if r {
            rex |= 0x04;
        }
        if x {
            rex |= 0x02;
        }
        if b {
            rex |= 0x01;
        }
        if rex != 0x40 {
            self.buf.emit_u8(rex);
        }
    }

    fn rex_rr(&mut self, w: bool, reg: Reg, rm: Reg) {
        self.rex_bits(w, reg.is_extended(), false, rm.is_extended());
    }

    fn rex_rm_disp32(&mut self, w: bool, reg: Reg, base: Reg) {
        self.rex_bits(w, reg.is_extended(), false, base.is_extended());
    }

    fn rex_only_b(&mut self, w: bool, rm: Reg) {
        self.rex_bits(w, false, false, rm.is_extended());
    }

    fn modrm_rr(&mut self, reg: Reg, rm: Reg) {
        self.buf.emit_u8(0xC0 | (reg.low3() << 3) | rm.low3());
    }

    fn modrm_rm_disp32(&mut self, reg: Reg, base: Reg, offset: u32) {
        self.buf.emit_u8(0x80 | (reg.low3() << 3) | base.low3());
        if matches!(base, Reg::Rsp | Reg::R12) {
            self.buf.emit_u8(0x24);
        }
        self.buf.emit_u32_le(offset);
    }

    fn modrm_group_reg_disp32(&mut self, group: u8, base: Reg, offset: u32) {
        self.buf.emit_u8(0x80 | ((group & 0x7) << 3) | base.low3());
        if matches!(base, Reg::Rsp | Reg::R12) {
            self.buf.emit_u8(0x24);
        }
        self.buf.emit_u32_le(offset);
    }

    /// `ret`
    pub fn ret(&mut self) {
        self.buf.emit_u8(0xC3);
    }

    /// `nop`
    pub fn nop(&mut self) {
        self.buf.emit_u8(0x90);
    }

    /// Save the callee-saved registers used by the template baseline:
    /// `rbx`, `r12`, `r13`, `r14`.
    pub fn push_callee_saved(&mut self) {
        self.push(Reg::Rbx);
        self.push(Reg::R12);
        self.push(Reg::R13);
        self.push(Reg::R14);
    }

    /// Restore the callee-saved registers saved by [`push_callee_saved`](Self::push_callee_saved).
    pub fn pop_callee_saved(&mut self) {
        self.pop(Reg::R14);
        self.pop(Reg::R13);
        self.pop(Reg::R12);
        self.pop(Reg::Rbx);
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

    /// `mov dst, imm64`
    pub fn mov_imm64(&mut self, dst: Reg, imm: u64) {
        self.rex_only_b(true, dst);
        self.buf.emit_u8(0xB8 + dst.low3());
        self.buf.emit_u64_le(imm);
    }

    /// `mov dst, src` (64-bit register move).
    pub fn mov_rr(&mut self, dst: Reg, src: Reg) {
        self.rex_rr(true, src, dst);
        self.buf.emit_u8(0x89);
        self.modrm_rr(src, dst);
    }

    /// `mov dst32, src32` (zero-extends into `dst`).
    pub fn mov_rr32(&mut self, dst: Reg, src: Reg) {
        self.rex_rr(false, src, dst);
        self.buf.emit_u8(0x89);
        self.modrm_rr(src, dst);
    }

    /// `mov dst, [base + offset]` (64-bit load).
    pub fn mov_mr_u32(&mut self, dst: Reg, base: Reg, offset: u32) {
        self.rex_rm_disp32(true, dst, base);
        self.buf.emit_u8(0x8B);
        self.modrm_rm_disp32(dst, base, offset);
    }

    /// `mov [base + offset], src` (64-bit store).
    pub fn mov_rm_u32(&mut self, base: Reg, offset: u32, src: Reg) {
        self.rex_rm_disp32(true, src, base);
        self.buf.emit_u8(0x89);
        self.modrm_rm_disp32(src, base, offset);
    }

    /// `mov dst32, [base + offset]` (32-bit load, zero-extending).
    pub fn mov_mr_u32_32(&mut self, dst: Reg, base: Reg, offset: u32) {
        self.rex_rm_disp32(false, dst, base);
        self.buf.emit_u8(0x8B);
        self.modrm_rm_disp32(dst, base, offset);
    }

    /// `mov [base + offset], src32` (32-bit store).
    pub fn mov_rm_u32_32(&mut self, base: Reg, offset: u32, src: Reg) {
        self.rex_rm_disp32(false, src, base);
        self.buf.emit_u8(0x89);
        self.modrm_rm_disp32(src, base, offset);
    }

    /// `add dst, rhs` with `dst = lhs + rhs`.
    pub fn add_rrr(&mut self, dst: Reg, lhs: Reg, rhs: Reg) {
        if dst != lhs {
            self.mov_rr(dst, lhs);
        }
        self.rex_rr(true, rhs, dst);
        self.buf.emit_u8(0x01);
        self.modrm_rr(rhs, dst);
    }

    /// `sub dst, rhs` with `dst = lhs - rhs`.
    pub fn sub_rrr(&mut self, dst: Reg, lhs: Reg, rhs: Reg) {
        if dst != lhs {
            self.mov_rr(dst, lhs);
        }
        self.rex_rr(true, rhs, dst);
        self.buf.emit_u8(0x29);
        self.modrm_rr(rhs, dst);
    }

    /// `imul dst, rhs` with `dst = lhs * rhs`.
    pub fn mul_rrr(&mut self, dst: Reg, lhs: Reg, rhs: Reg) {
        if dst != lhs {
            self.mov_rr(dst, lhs);
        }
        self.rex_rr(true, dst, rhs);
        self.buf.emit_u8(0x0F);
        self.buf.emit_u8(0xAF);
        self.modrm_rr(dst, rhs);
    }

    /// `xor dst, rhs` with `dst = lhs XOR rhs`.
    pub fn eor_rrr(&mut self, dst: Reg, lhs: Reg, rhs: Reg) {
        if dst != lhs {
            self.mov_rr(dst, lhs);
        }
        self.rex_rr(true, rhs, dst);
        self.buf.emit_u8(0x31);
        self.modrm_rr(rhs, dst);
    }

    /// `and dst, rhs` with `dst = lhs AND rhs`.
    pub fn and_rrr(&mut self, dst: Reg, lhs: Reg, rhs: Reg) {
        if dst != lhs {
            self.mov_rr(dst, lhs);
        }
        self.rex_rr(true, rhs, dst);
        self.buf.emit_u8(0x21);
        self.modrm_rr(rhs, dst);
    }

    /// `or dst, rhs` with `dst = lhs OR rhs`.
    pub fn orr_rrr(&mut self, dst: Reg, lhs: Reg, rhs: Reg) {
        if dst != lhs {
            self.mov_rr(dst, lhs);
        }
        self.rex_rr(true, rhs, dst);
        self.buf.emit_u8(0x09);
        self.modrm_rr(rhs, dst);
    }

    /// `cmp lhs, rhs` (64-bit).
    pub fn cmp_rr(&mut self, lhs: Reg, rhs: Reg) {
        self.rex_rr(true, rhs, lhs);
        self.buf.emit_u8(0x39);
        self.modrm_rr(rhs, lhs);
    }

    /// `test lhs, rhs` (64-bit).
    pub fn test_rr(&mut self, lhs: Reg, rhs: Reg) {
        self.rex_rr(true, rhs, lhs);
        self.buf.emit_u8(0x85);
        self.modrm_rr(rhs, lhs);
    }

    /// `shr reg, imm8` (64-bit).
    pub fn shr_ri(&mut self, reg: Reg, imm: u8) {
        self.rex_only_b(true, reg);
        self.buf.emit_u8(0xC1);
        self.modrm_rr(Reg::Rbp, reg);
        self.buf.emit_u8(imm);
    }

    /// `shl reg32, imm8` (32-bit; upper half zero-extended by the ISA).
    pub fn shl_r32_i(&mut self, reg: Reg, imm: u8) {
        self.rex_only_b(false, reg);
        self.buf.emit_u8(0xC1);
        self.modrm_rr(Reg::Rsp, reg);
        self.buf.emit_u8(imm);
    }

    /// `sar reg32, imm8` (32-bit arithmetic shift right).
    pub fn sar_r32_i(&mut self, reg: Reg, imm: u8) {
        self.rex_only_b(false, reg);
        self.buf.emit_u8(0xC1);
        self.modrm_rr(Reg::Rdi, reg);
        self.buf.emit_u8(imm);
    }

    /// `not reg` (64-bit).
    pub fn not_r(&mut self, reg: Reg) {
        self.rex_only_b(true, reg);
        self.buf.emit_u8(0xF7);
        self.modrm_rr(Reg::Rdx, reg);
    }

    /// `movsxd dst, src32`
    pub fn sxtw(&mut self, dst: Reg, src: Reg) {
        self.rex_rr(true, dst, src);
        self.buf.emit_u8(0x63);
        self.modrm_rr(dst, src);
    }

    /// Fast int32 tag check using a preloaded tag register.
    ///
    /// Emits:
    ///
    /// ```text
    /// mov scratch, src
    /// xor scratch, tag_reg
    /// shr scratch, 32
    /// ```
    ///
    /// The caller branches on `Ne`/`Eq` immediately afterwards; `shr`
    /// sets ZF when the upper 32 bits of `src XOR tag_reg` are zero,
    /// which is exactly the NaN-box int32 tag-match predicate.
    pub fn check_int32_tag_fast(&mut self, src: Reg, scratch: Reg, tag_reg: Reg) {
        self.mov_rr(scratch, src);
        self.eor_rrr(scratch, scratch, tag_reg);
        self.shr_ri(scratch, 32);
    }

    /// Box an int32 from `src` into a NaN-boxed value in `dst`.
    pub fn box_int32(&mut self, dst: Reg, src: Reg) {
        use otter_vm::value::TAG_INT32;

        let scratch = if dst == Reg::Rdx { Reg::Rcx } else { Reg::Rdx };
        self.mov_imm64(dst, TAG_INT32);
        self.mov_rr32(scratch, src);
        self.orr_rrr(dst, dst, scratch);
    }

    /// Emit `jmp rel32` and return the instruction start offset.
    pub fn b_placeholder(&mut self) -> u32 {
        let pos = self.position();
        self.buf.emit_u8(0xE9);
        self.buf.emit_u32_le(0);
        pos
    }

    /// Emit `jcc rel32` and return the instruction start offset.
    pub fn b_cond_placeholder(&mut self, cond: Cond) -> u32 {
        let pos = self.position();
        self.buf.emit_u8(0x0F);
        self.buf.emit_u8(cond as u8);
        self.buf.emit_u32_le(0);
        pos
    }

    /// `mov [base + offset], imm32`
    pub fn mov_mem_imm32(&mut self, base: Reg, offset: u32, imm: u32) {
        self.rex_only_b(false, base);
        self.buf.emit_u8(0xC7);
        self.modrm_group_reg_disp32(0, base, offset);
        self.buf.emit_u32_le(imm);
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
        assert_eq!(buf.bytes(), &[0x41, 0x54]);
    }

    #[test]
    fn test_emit_mov_imm64() {
        let mut buf = CodeBuffer::new();
        let mut asm = Assembler::new(&mut buf);
        asm.mov_imm64(Reg::Rax, 0xDEADBEEF_CAFEBABE);
        assert_eq!(buf.len(), 10);
        assert_eq!(buf.bytes()[0], 0x48);
        assert_eq!(buf.bytes()[1], 0xB8);
    }

    #[test]
    fn test_emit_nop() {
        let mut buf = CodeBuffer::new();
        let mut asm = Assembler::new(&mut buf);
        asm.nop();
        assert_eq!(buf.bytes(), &[0x90]);
    }

    #[test]
    fn test_check_int32_tag_fast_emits_code() {
        let mut buf = CodeBuffer::new();
        let mut asm = Assembler::new(&mut buf);
        asm.check_int32_tag_fast(Reg::Rsi, Reg::Rax, Reg::R14);
        assert!(
            buf.len() >= 10,
            "fast tag check should emit multiple instructions"
        );
    }

    #[test]
    fn test_branch_placeholders_emit_expected_lengths() {
        let mut buf = CodeBuffer::new();
        let mut asm = Assembler::new(&mut buf);
        let jmp = asm.b_placeholder();
        let jne = asm.b_cond_placeholder(Cond::Ne);
        assert_eq!(jmp, 0);
        assert_eq!(jne, 5);
        assert_eq!(buf.len(), 11);
    }
}
