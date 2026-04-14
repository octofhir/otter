//! AArch64 template assembler.
//!
//! Direct instruction emission for the hot subset used by the baseline JIT.

use crate::arch::CodeBuffer;

/// AArch64 general-purpose registers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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
    X29 = 29,
    X30 = 30,
    Sp = 31,
    Xzr = 32, // Used for encoding zero register
}

impl Reg {
    pub fn encoding(self) -> u32 {
        if self == Reg::Xzr {
            31
        } else {
            self as u32
        }
    }
}

/// AArch64 register aliases for template emission.
pub mod regs {
    pub use super::Reg::*;
    pub const LR: super::Reg = super::Reg::X30;
    pub const ZZR: super::Reg = super::Reg::Xzr;
}

/// Conditional branch codes. Signed variants for int32 JS semantics.
///
/// Reference: Arm Architecture Reference Manual, C1.2.4 — Condition codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Cond {
    Eq = 0x0,
    Ne = 0x1,
    Ge = 0xA, // signed >=
    Lt = 0xB, // signed <
    Gt = 0xC, // signed >
    Le = 0xD, // signed <=
}

pub struct Assembler<'a> {
    pub buf: &'a mut CodeBuffer,
}

impl<'a> Assembler<'a> {
    pub fn new(buf: &'a mut CodeBuffer) -> Self {
        Self { buf }
    }

    pub fn position(&self) -> u32 {
        self.buf.len() as u32
    }

    fn emit_insn(&mut self, insn: u32) {
        self.buf.emit_u32_le(insn);
    }

    /// `ret`
    pub fn ret(&mut self) {
        self.emit_insn(0xD65F03C0);
    }

    /// `nop`
    pub fn nop(&mut self) {
        self.emit_insn(0xD503201F);
    }

    /// `mov xd, xn` (alias for `orr xd, xzr, xn`)
    pub fn mov_rr(&mut self, dst: Reg, src: Reg) {
        let insn = 0xAA0003E0 | (src.encoding() << 16) | dst.encoding();
        self.emit_insn(insn);
    }

    /// `stp x19, x30, [sp, #-16]!` (push x19 and lr; pre-indexed -16).
    /// SP decreases by 16 (one register pair), matched 1:1 by [`pop_x19_lr`].
    pub fn push_x19_lr(&mut self) {
        self.emit_insn(0xA9BF7BF3);
    }

    /// `ldp x19, x30, [sp], #16` (pop x19 and lr; post-indexed +16).
    ///
    /// Must increment SP by exactly 16 to match [`push_x19_lr`]'s
    /// 16-byte allocation. The previous encoding (`0xA8C27BF3`, post-index
    /// `#32`) leaked 16 bytes of stack on every JIT entry — invisible for
    /// the once-per-script top-level dispatch, but catastrophic once Phase A
    /// tier-up routes inner-function calls through the same prologue
    /// hundreds of times per script. Symptoms were stack scribbling that
    /// surfaced as a hang in `arithmetic_loop.ts` after warmup.
    pub fn pop_x19_lr(&mut self) {
        self.emit_insn(0xA8C17BF3);
    }

    /// `stp x19, lr, [sp, #-32]!` — allocate a 32-byte frame and save
    /// x19/lr at offset 0. Caller must also save x20 (usually via
    /// [`str_x20_at_sp16`]). Paired with [`pop_x19_lr_32`].
    ///
    /// Encoding: pre-indexed STP with imm7 = -4 (scaled by 8 = -32 bytes).
    pub fn push_x19_lr_32(&mut self) {
        // 0xA9800000 base | (0x7C << 15) | (30<<10) | (31<<5) | 19
        self.emit_insn(0xA9BE7BF3);
    }

    /// `str x20, [sp, #16]` — save x20 into the reserved slot of a
    /// 32-byte frame. Call right after [`push_x19_lr_32`].
    pub fn str_x20_at_sp16(&mut self) {
        // 0xF9000000 | (imm12=2 << 10) | (Rn=sp=31 << 5) | Rt=20
        self.emit_insn(0xF9000BF4);
    }

    /// `ldr x20, [sp, #16]` — restore x20 from the 32-byte frame.
    pub fn ldr_x20_at_sp16(&mut self) {
        // 0xF9400000 | (imm12=2 << 10) | (Rn=sp=31 << 5) | Rt=20
        self.emit_insn(0xF9400BF4);
    }

    /// `ldp x19, lr, [sp], #32` — tear down the 32-byte frame, restoring
    /// x19/lr. Must be preceded by [`ldr_x20_at_sp16`] if x20 was saved.
    ///
    /// Encoding: post-indexed LDP with imm7 = +4 (scaled by 8 = +32).
    pub fn pop_x19_lr_32(&mut self) {
        self.emit_insn(0xA8C27BF3);
    }

    /// `b offset` (placeholder for 26-bit immediate branch)
    pub fn b_placeholder(&mut self) -> u32 {
        let pos = self.position();
        self.emit_insn(0x14000000);
        pos
    }

    /// `b.cond offset` (placeholder for 19-bit immediate conditional branch)
    pub fn b_cond_placeholder(&mut self, cond: Cond) -> u32 {
        let pos = self.position();
        let insn = 0x54000000 | (cond as u32);
        self.emit_insn(insn);
        pos
    }

    /// `blr xn` (branch with link to register)
    pub fn blr(&mut self, reg: Reg) {
        let insn = 0xD63F0000 | (reg.encoding() << 5);
        self.emit_insn(insn);
    }

    /// `movz dst, imm16, lsl #shift` (move 16-bit immediate with zero)
    /// Shift must be 0, 16, 32, or 48.
    pub fn movz(&mut self, dst: Reg, imm16: u16, shift: u8) {
        let hw = (shift / 16) as u32;
        let insn = 0xD2800000 | (hw << 21) | ((imm16 as u32) << 5) | dst.encoding();
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
        self.movz(dst, imm as u16, 0);
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

    pub fn str_u64_imm(&mut self, src: Reg, base: Reg, byte_offset: u32) {
        assert!(byte_offset % 8 == 0, "64-bit offset must be 8-byte aligned");
        let imm12 = byte_offset / 8;
        let insn = 0xF9000000 | (imm12 << 10) | (base.encoding() << 5) | src.encoding();
        self.emit_insn(insn);
    }

    pub fn ldr_u64_imm(&mut self, dst: Reg, base: Reg, byte_offset: u32) {
        assert!(byte_offset % 8 == 0, "64-bit offset must be 8-byte aligned");
        let imm12 = byte_offset / 8;
        let insn = 0xF9400000 | (imm12 << 10) | (base.encoding() << 5) | dst.encoding();
        self.emit_insn(insn);
    }

    pub fn str_u32_imm(&mut self, src: Reg, base: Reg, byte_offset: u32) {
        assert!(byte_offset % 4 == 0, "32-bit offset must be 4-byte aligned");
        let imm12 = byte_offset / 4;
        let insn = 0xB9000000 | (imm12 << 10) | (base.encoding() << 5) | src.encoding();
        self.emit_insn(insn);
    }

    pub fn ldr_u32_imm(&mut self, dst: Reg, base: Reg, byte_offset: u32) {
        assert!(byte_offset % 4 == 0, "32-bit offset must be 4-byte aligned");
        let imm12 = byte_offset / 4;
        let insn = 0xB9400000 | (imm12 << 10) | (base.encoding() << 5) | dst.encoding();
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

    /// Clobbers x14, x15.
    pub fn check_object_tag(&mut self, src: Reg) {
        use otter_vm::value::{OBJECT_TAG_MASK, TAG_PTR_OBJECT};
        self.mov_imm64(Reg::X15, OBJECT_TAG_MASK);
        self.and_rr(Reg::X14, src, Reg::X15);
        self.mov_imm64(Reg::X15, TAG_PTR_OBJECT);
        self.cmp_rr(Reg::X14, Reg::X15);
    }

    /// Sets flags to `Eq` if `src` is a NaN-boxed int32. Clobbers x14, x15.
    /// Callers inspect the `Ne` condition to branch to the deopt pad.
    ///
    /// Old (verbose, 12 insns): two `mov_imm64` chains + `and` + `cmp`.
    pub fn check_int32_tag(&mut self, src: Reg) {
        use otter_vm::value::{INT32_TAG_MASK, TAG_INT32};
        self.mov_imm64(Reg::X15, INT32_TAG_MASK);
        self.and_rr(Reg::X14, src, Reg::X15);
        self.mov_imm64(Reg::X15, TAG_INT32);
        self.cmp_rr(Reg::X14, Reg::X15);
    }

    /// Fast int32 tag check, reading the pre-loaded `TAG_INT32` out of the
    /// callee-saved `tag_reg` (templates pin this to x20 at prologue).
    /// Clobbers x14. Sets flags so `Ne` means "not an int32". **3 insns**
    /// vs the legacy [`check_int32_tag`]'s 12.
    ///
    /// Encoding rationale: `TAG_INT32 = 0x7FF8_0001_0000_0000` has the
    /// discriminator packed entirely into the upper 32 bits, so
    /// `src XOR tag_reg` is zero in the upper 32 exactly when the tag
    /// matches. `tst xN, #0xffff_ffff_0000_0000` is a valid 64-bit logical
    /// immediate (one contiguous run of 32 set bits in the upper half,
    /// AArch64 encoding `N=1, imms=0x1F, immr=0x20`).
    pub fn check_int32_tag_fast(&mut self, src: Reg, tag_reg: Reg) {
        // x14 = src XOR tag_reg (upper 32 bits == 0 iff tag matches).
        self.eor_rrr(Reg::X14, src, tag_reg);
        // tst x14, #0xFFFF_FFFF_0000_0000. ANDS XZR, x14, #imm:
        //   N=1 (bit 22), immr=32 (bits 21:16 = 100000),
        //   imms=31 (bits 15:10 = 011111).
        let insn = 0xF240_0000
            | (32u32 << 16)
            | (31u32 << 10)
            | (Reg::X14.encoding() << 5)
            | Reg::Xzr.encoding();
        self.emit_insn(insn);
    }

    /// `orr xd, xn, xm` (bitwise OR, 64-bit, shifted register form, no shift)
    pub fn orr_rrr(&mut self, dst: Reg, lhs: Reg, rhs: Reg) {
        let insn = 0xAA000000 | (rhs.encoding() << 16) | (lhs.encoding() << 5) | dst.encoding();
        self.emit_insn(insn);
    }

    /// `eor xd, xn, xm` (bitwise XOR, 64-bit, shifted register form)
    pub fn eor_rrr(&mut self, dst: Reg, lhs: Reg, rhs: Reg) {
        let insn = 0xCA000000 | (rhs.encoding() << 16) | (lhs.encoding() << 5) | dst.encoding();
        self.emit_insn(insn);
    }

    /// `and xd, xn, xm` (bitwise AND, 64-bit shifted-register form — not the
    /// immediate form used by `and_rr`). Prefer this over `and_rr` when both
    /// operands are general-purpose registers.
    pub fn and_rrr(&mut self, dst: Reg, lhs: Reg, rhs: Reg) {
        let insn = 0x8A000000 | (rhs.encoding() << 16) | (lhs.encoding() << 5) | dst.encoding();
        self.emit_insn(insn);
    }

    /// `lsl wd, wn, wm` (32-bit logical shift left by register amount —
    /// masks shift by 31, matches JS `<<` semantics when both operands are
    /// extracted int32 payloads).
    pub fn lslv_w(&mut self, dst: Reg, lhs: Reg, rhs: Reg) {
        let insn = 0x1AC02000 | (rhs.encoding() << 16) | (lhs.encoding() << 5) | dst.encoding();
        self.emit_insn(insn);
    }

    /// `lsr wd, wn, wm` (32-bit logical shift right by register amount —
    /// matches JS `>>>` when `lhs` already fits in 32 bits unsigned).
    pub fn lsrv_w(&mut self, dst: Reg, lhs: Reg, rhs: Reg) {
        let insn = 0x1AC02400 | (rhs.encoding() << 16) | (lhs.encoding() << 5) | dst.encoding();
        self.emit_insn(insn);
    }

    /// `asr wd, wn, wm` (32-bit arithmetic shift right — matches JS `>>`
    /// when `lhs` is a sign-extended int32).
    pub fn asrv_w(&mut self, dst: Reg, lhs: Reg, rhs: Reg) {
        let insn = 0x1AC02800 | (rhs.encoding() << 16) | (lhs.encoding() << 5) | dst.encoding();
        self.emit_insn(insn);
    }

    /// `sxtw xd, wn` — sign-extend 32-bit to 64-bit. Used before ASR so the
    /// shift preserves the sign of the 32-bit JS value.
    pub fn sxtw(&mut self, dst: Reg, src: Reg) {
        // SBFM Xd, Xn, #0, #31
        let insn = 0x93407C00 | (src.encoding() << 5) | dst.encoding();
        self.emit_insn(insn);
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
        let mut buf = crate::arch::CodeBuffer::new();
        let mut asm = Assembler::new(&mut buf);
        asm.ret();
        assert_eq!(buf.len(), 4);
        assert_eq!(buf.bytes(), &[0xC0, 0x03, 0x5F, 0xD6]); // RET in LE
    }

    #[test]
    fn test_emit_nop() {
        let mut buf = crate::arch::CodeBuffer::new();
        let mut asm = Assembler::new(&mut buf);
        asm.nop();
        assert_eq!(buf.len(), 4);
        assert_eq!(buf.bytes(), &[0x1F, 0x20, 0x03, 0xD5]); // NOP in LE
    }

    #[test]
    fn test_emit_movz() {
        let mut buf = crate::arch::CodeBuffer::new();
        let mut asm = Assembler::new(&mut buf);
        asm.movz(Reg::X0, 42, 0);
        assert_eq!(buf.len(), 4);
        // MOVZ X0, #42: 0xD2800540
        let insn = u32::from_le_bytes(buf.bytes().try_into().unwrap());
        assert_eq!(insn, 0xD2800540);
    }
}
