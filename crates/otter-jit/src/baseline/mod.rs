//! Template baseline Tier 1 candidate analysis.
//!
//! This module is the first step toward a direct `bytecode -> asm` baseline
//! compiler. It does not emit machine code yet; instead, it recognizes a
//! narrow, hot subset of bytecode that can be lowered without MIR/CLIF.

use crate::arch::CodeBuffer;
use otter_vm::bytecode::{Instruction, Opcode};
use otter_vm::module::Function;

/// A bytecode operation supported by the template baseline path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TemplateInstruction {
    LoadI32 { dst: u16, imm: i32 },
    Move { dst: u16, src: u16 },
    AddI32 { dst: u16, lhs: u16, rhs: u16 },
    SubI32 { dst: u16, lhs: u16, rhs: u16 },
    MulI32 { dst: u16, lhs: u16, rhs: u16 },
    LtI32 { dst: u16, lhs: u16, rhs: u16 },
    Jump { target_pc: u32 },
    JumpIfFalse { cond: u16, target_pc: u32 },
    Return { src: u16 },
}

/// A template-baseline candidate function.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateProgram {
    /// Function name for diagnostics.
    pub function_name: String,
    /// Total register count in the shared frame layout.
    pub register_count: u16,
    /// Instructions in template-friendly form.
    pub instructions: Vec<TemplateInstruction>,
    /// Loop headers detected from backward branches.
    pub loop_headers: Vec<u32>,
}

/// Why a function is not yet supported by the template baseline Tier 1 path.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TemplateCompileError {
    #[error("unsupported opcode at pc {pc}: {opcode:?}")]
    UnsupportedOpcode { pc: u32, opcode: Opcode },
    #[error("jump target out of range at pc {pc}: offset={offset}")]
    InvalidJumpTarget { pc: u32, offset: i32 },
}

/// Why stencil emission failed for an otherwise recognized template program.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TemplateEmitError {
    #[error("unsupported host architecture for template emission: {0}")]
    UnsupportedHostArch(&'static str),
    #[error("register slot offset out of range for template emission: slot={slot}")]
    RegisterSlotOutOfRange { slot: u16 },
    #[error("unsupported template sequence at pc {pc}: {detail}")]
    UnsupportedSequence { pc: u32, detail: &'static str },
    #[error("branch target out of range for template emission: from={source_offset} to pc={target_pc}")]
    BranchTargetOutOfRange { source_offset: u32, target_pc: u32 },
}

/// Analyze whether a function can be compiled by the template baseline path.
///
/// The current supported subset is intentionally narrow:
/// `LoadI32`, `Move`, `Add`, `Sub`, `Mul`, `Lt`, `Jump`, `JumpIfFalse`, `Return`.
pub fn analyze_template_candidate(
    function: &Function,
) -> Result<TemplateProgram, TemplateCompileError> {
    let instructions = function.bytecode().instructions();
    let mut lowered = Vec::with_capacity(instructions.len());
    let mut loop_headers = Vec::new();

    for (pc, instruction) in instructions.iter().enumerate() {
        let pc = pc as u32;
        lowered.push(lower_instruction(pc, *instruction)?);

        match instruction.opcode() {
            Opcode::Jump | Opcode::JumpIfFalse => {
                let target_pc = resolve_target_pc(pc, instruction.immediate_i32())
                    .ok_or(TemplateCompileError::InvalidJumpTarget {
                        pc,
                        offset: instruction.immediate_i32(),
                    })?;
                if target_pc <= pc && !loop_headers.contains(&target_pc) {
                    loop_headers.push(target_pc);
                }
            }
            _ => {}
        }
    }

    Ok(TemplateProgram {
        function_name: function.name().unwrap_or("<anonymous>").to_string(),
        register_count: function.frame_layout().register_count(),
        instructions: lowered,
        loop_headers,
    })
}

/// Emit an architecture-specific baseline stencil for a recognized template program.
///
/// This is a code-buffer generator, not yet an installed executable function.
/// The first implementation targets the host `aarch64` baseline subset used by
/// hot arithmetic loops.
pub fn emit_template_stencil(program: &TemplateProgram) -> Result<CodeBuffer, TemplateEmitError> {
    #[cfg(target_arch = "aarch64")]
    {
        emit_template_stencil_aarch64(program)
    }
    #[cfg(target_arch = "x86_64")]
    {
        Err(TemplateEmitError::UnsupportedHostArch("x86_64"))
    }
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    {
        Err(TemplateEmitError::UnsupportedHostArch(std::env::consts::ARCH))
    }
}

#[cfg(target_arch = "aarch64")]
fn emit_template_stencil_aarch64(program: &TemplateProgram) -> Result<CodeBuffer, TemplateEmitError> {
    use crate::arch::aarch64::{Assembler, Cond, Reg};
    use crate::codegen::value_repr::TAG_INT32;

    #[derive(Debug, Clone, Copy)]
    enum BranchKind {
        Unconditional,
        Ge,
    }

    #[derive(Debug, Clone, Copy)]
    struct BranchPatch {
        source_offset: u32,
        target_pc: u32,
        kind: BranchKind,
    }

    fn slot_offset(slot: u16) -> Result<u32, TemplateEmitError> {
        let byte_offset = u32::from(slot) * 8;
        if byte_offset > (4095 * 8) {
            return Err(TemplateEmitError::RegisterSlotOutOfRange { slot });
        }
        Ok(byte_offset)
    }

    let mut buf = CodeBuffer::new();
    let mut asm = Assembler::new(&mut buf);
    let mut pc_offsets = vec![0_u32; program.instructions.len()];
    let mut patches = Vec::new();

    // x0 = JitContext*. Load registers_base once into x9.
    asm.ldr_u64_imm(Reg::X9, Reg::X0, 0);

    let mut pc = 0usize;
    while pc < program.instructions.len() {
        pc_offsets[pc] = asm.position();

        match &program.instructions[pc] {
            TemplateInstruction::LoadI32 { dst, imm } => {
                let boxed = TAG_INT32 | u64::from(*imm as u32);
                asm.mov_imm64(Reg::X10, boxed);
                asm.str_u64_imm(Reg::X10, Reg::X9, slot_offset(*dst)?);
            }
            TemplateInstruction::Move { dst, src } => {
                asm.ldr_u64_imm(Reg::X10, Reg::X9, slot_offset(*src)?);
                asm.str_u64_imm(Reg::X10, Reg::X9, slot_offset(*dst)?);
            }
            TemplateInstruction::AddI32 { dst, lhs, rhs } => {
                asm.ldr_u64_imm(Reg::X10, Reg::X9, slot_offset(*lhs)?);
                asm.ldr_u64_imm(Reg::X11, Reg::X9, slot_offset(*rhs)?);
                asm.extract_int32(Reg::X10, Reg::X10);
                asm.extract_int32(Reg::X11, Reg::X11);
                asm.add_rrr(Reg::X10, Reg::X10, Reg::X11);
                asm.box_int32(Reg::X10, Reg::X10);
                asm.str_u64_imm(Reg::X10, Reg::X9, slot_offset(*dst)?);
            }
            TemplateInstruction::SubI32 { dst, lhs, rhs } => {
                asm.ldr_u64_imm(Reg::X10, Reg::X9, slot_offset(*lhs)?);
                asm.ldr_u64_imm(Reg::X11, Reg::X9, slot_offset(*rhs)?);
                asm.extract_int32(Reg::X10, Reg::X10);
                asm.extract_int32(Reg::X11, Reg::X11);
                asm.sub_rrr(Reg::X10, Reg::X10, Reg::X11);
                asm.box_int32(Reg::X10, Reg::X10);
                asm.str_u64_imm(Reg::X10, Reg::X9, slot_offset(*dst)?);
            }
            TemplateInstruction::MulI32 { dst, lhs, rhs } => {
                asm.ldr_u64_imm(Reg::X10, Reg::X9, slot_offset(*lhs)?);
                asm.ldr_u64_imm(Reg::X11, Reg::X9, slot_offset(*rhs)?);
                asm.extract_int32(Reg::X10, Reg::X10);
                asm.extract_int32(Reg::X11, Reg::X11);
                asm.mul_rrr(Reg::X10, Reg::X10, Reg::X11);
                asm.box_int32(Reg::X10, Reg::X10);
                asm.str_u64_imm(Reg::X10, Reg::X9, slot_offset(*dst)?);
            }
            TemplateInstruction::LtI32 { lhs, rhs, .. } => {
                let Some(TemplateInstruction::JumpIfFalse { target_pc, .. }) =
                    program.instructions.get(pc + 1)
                else {
                    return Err(TemplateEmitError::UnsupportedSequence {
                        pc: pc as u32,
                        detail: "`LtI32` currently requires immediate `JumpIfFalse` fusion",
                    });
                };

                asm.ldr_u64_imm(Reg::X10, Reg::X9, slot_offset(*lhs)?);
                asm.ldr_u64_imm(Reg::X11, Reg::X9, slot_offset(*rhs)?);
                asm.extract_int32(Reg::X10, Reg::X10);
                asm.extract_int32(Reg::X11, Reg::X11);
                asm.cmp_rr(Reg::X10, Reg::X11);
                let branch = asm.b_cond_placeholder(Cond::Ge);
                patches.push(BranchPatch {
                    source_offset: branch,
                    target_pc: *target_pc,
                    kind: BranchKind::Ge,
                });

                if pc + 1 < pc_offsets.len() {
                    pc_offsets[pc + 1] = asm.position();
                }
                pc += 1;
            }
            TemplateInstruction::JumpIfFalse { .. } => {
                return Err(TemplateEmitError::UnsupportedSequence {
                    pc: pc as u32,
                    detail: "standalone `JumpIfFalse` is not yet supported; use compare/branch fusion",
                });
            }
            TemplateInstruction::Jump { target_pc } => {
                let branch = asm.b_placeholder();
                patches.push(BranchPatch {
                    source_offset: branch,
                    target_pc: *target_pc,
                    kind: BranchKind::Unconditional,
                });
            }
            TemplateInstruction::Return { src } => {
                asm.ldr_u64_imm(Reg::X0, Reg::X9, slot_offset(*src)?);
                asm.ret();
            }
        }

        pc += 1;
    }

    for patch in patches {
        let Some(&target_offset) = pc_offsets.get(patch.target_pc as usize) else {
            return Err(TemplateEmitError::BranchTargetOutOfRange {
                source_offset: patch.source_offset,
                target_pc: patch.target_pc,
            });
        };
        let delta = i64::from(target_offset) - i64::from(patch.source_offset);
        if delta % 4 != 0 {
            return Err(TemplateEmitError::BranchTargetOutOfRange {
                source_offset: patch.source_offset,
                target_pc: patch.target_pc,
            });
        }
        let Some(existing) = buf.read_u32_le(patch.source_offset) else {
            return Err(TemplateEmitError::BranchTargetOutOfRange {
                source_offset: patch.source_offset,
                target_pc: patch.target_pc,
            });
        };
        let patched = match patch.kind {
            BranchKind::Unconditional => {
                let imm26 = ((delta / 4) as i32 as u32) & 0x03FF_FFFF;
                existing | imm26
            }
            BranchKind::Ge => {
                let imm19 = ((delta / 4) as i32 as u32) & 0x0007_FFFF;
                existing | (imm19 << 5)
            }
        };
        if !buf.patch_u32_le(patch.source_offset, patched) {
            return Err(TemplateEmitError::BranchTargetOutOfRange {
                source_offset: patch.source_offset,
                target_pc: patch.target_pc,
            });
        }
    }

    Ok(buf)
}

fn lower_instruction(pc: u32, instruction: Instruction) -> Result<TemplateInstruction, TemplateCompileError> {
    match instruction.opcode() {
        Opcode::LoadI32 => Ok(TemplateInstruction::LoadI32 {
            dst: instruction.a(),
            imm: instruction.immediate_i32(),
        }),
        Opcode::Move => Ok(TemplateInstruction::Move {
            dst: instruction.a(),
            src: instruction.b(),
        }),
        Opcode::Add => Ok(TemplateInstruction::AddI32 {
            dst: instruction.a(),
            lhs: instruction.b(),
            rhs: instruction.c(),
        }),
        Opcode::Sub => Ok(TemplateInstruction::SubI32 {
            dst: instruction.a(),
            lhs: instruction.b(),
            rhs: instruction.c(),
        }),
        Opcode::Mul => Ok(TemplateInstruction::MulI32 {
            dst: instruction.a(),
            lhs: instruction.b(),
            rhs: instruction.c(),
        }),
        Opcode::Lt => Ok(TemplateInstruction::LtI32 {
            dst: instruction.a(),
            lhs: instruction.b(),
            rhs: instruction.c(),
        }),
        Opcode::Jump => Ok(TemplateInstruction::Jump {
            target_pc: resolve_target_pc(pc, instruction.immediate_i32())
                .ok_or(TemplateCompileError::InvalidJumpTarget {
                    pc,
                    offset: instruction.immediate_i32(),
                })?,
        }),
        Opcode::JumpIfFalse => Ok(TemplateInstruction::JumpIfFalse {
            cond: instruction.a(),
            target_pc: resolve_target_pc(pc, instruction.immediate_i32())
                .ok_or(TemplateCompileError::InvalidJumpTarget {
                    pc,
                    offset: instruction.immediate_i32(),
                })?,
        }),
        Opcode::Return => Ok(TemplateInstruction::Return { src: instruction.a() }),
        opcode => Err(TemplateCompileError::UnsupportedOpcode { pc, opcode }),
    }
}

fn resolve_target_pc(pc: u32, offset: i32) -> Option<u32> {
    let current = i64::from(pc);
    let target = current + 1 + i64::from(offset);
    u32::try_from(target).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use otter_vm::bytecode::{Bytecode, BytecodeRegister, JumpOffset};
    use otter_vm::frame::FrameLayout;

    fn loop_function() -> Function {
        Function::with_bytecode(
            Some("baseline_loop"),
            FrameLayout::new(0, 0, 0, 5).expect("layout"),
            Bytecode::from(vec![
                Instruction::load_i32(BytecodeRegister::new(0), 0),
                Instruction::load_i32(BytecodeRegister::new(1), 0),
                Instruction::load_i32(BytecodeRegister::new(2), 10),
                Instruction::load_i32(BytecodeRegister::new(4), 1),
                Instruction::lt(
                    BytecodeRegister::new(3),
                    BytecodeRegister::new(1),
                    BytecodeRegister::new(2),
                ),
                Instruction::jump_if_false(BytecodeRegister::new(3), JumpOffset::new(3)),
                Instruction::add(
                    BytecodeRegister::new(0),
                    BytecodeRegister::new(0),
                    BytecodeRegister::new(1),
                ),
                Instruction::add(
                    BytecodeRegister::new(1),
                    BytecodeRegister::new(1),
                    BytecodeRegister::new(4),
                ),
                Instruction::jump(JumpOffset::new(-5)),
                Instruction::ret(BytecodeRegister::new(0)),
            ]),
        )
    }

    #[test]
    fn template_analyzer_accepts_hot_loop_subset() {
        let function = loop_function();
        let program = analyze_template_candidate(&function).expect("subset should be accepted");

        assert_eq!(program.function_name, "baseline_loop");
        assert_eq!(program.register_count, 5);
        assert_eq!(program.instructions.len(), 10);
        assert_eq!(program.loop_headers, vec![4]);
        assert!(matches!(
            program.instructions[5],
            TemplateInstruction::JumpIfFalse {
                cond: 3,
                target_pc: 9
            }
        ));
    }

    #[test]
    fn template_analyzer_rejects_unsupported_opcode() {
        let function = Function::with_bytecode(
            Some("unsupported"),
            FrameLayout::new(0, 0, 0, 2).expect("layout"),
            Bytecode::from(vec![
                Instruction::new_object(BytecodeRegister::new(0)),
                Instruction::ret(BytecodeRegister::new(0)),
            ]),
        );

        let err = analyze_template_candidate(&function).expect_err("new_object is unsupported");
        assert_eq!(
            err,
            TemplateCompileError::UnsupportedOpcode {
                pc: 0,
                opcode: Opcode::NewObject,
            }
        );
    }

    #[cfg(target_arch = "aarch64")]
    #[test]
    fn template_emitter_produces_aarch64_stencil() {
        let function = loop_function();
        let program = analyze_template_candidate(&function).expect("subset should be accepted");
        let buf = emit_template_stencil(&program).expect("stencil emission should succeed");

        assert!(!buf.is_empty());
        assert_eq!(buf.len() % 4, 0, "aarch64 instructions are fixed-width");
        assert_eq!(&buf.bytes()[buf.len() - 4..], &[0xC0, 0x03, 0x5F, 0xD6]);
    }

    #[cfg(target_arch = "aarch64")]
    #[test]
    fn template_emitter_boxes_immediates_into_slots() {
        let function = Function::with_bytecode(
            Some("const_only"),
            FrameLayout::new(0, 0, 0, 1).expect("layout"),
            Bytecode::from(vec![
                Instruction::load_i32(BytecodeRegister::new(0), 7),
                Instruction::ret(BytecodeRegister::new(0)),
            ]),
        );

        let program = analyze_template_candidate(&function).expect("subset should be accepted");
        let buf = emit_template_stencil(&program).expect("stencil emission should succeed");
        assert!(buf.len() >= 16, "const+return stencil should not be tiny");
        assert_eq!(buf.len() % 4, 0, "aarch64 instructions are fixed-width");
        assert_eq!(&buf.bytes()[buf.len() - 4..], &[0xC0, 0x03, 0x5F, 0xD6]);
    }
}
