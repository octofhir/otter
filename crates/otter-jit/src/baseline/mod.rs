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
    CallDirect { dst: u16, callee_fn_idx: u32, arg_base: u16, arg_count: u16 },
    GetPropShaped { dst: u16, obj: u16, shape_id: u64, slot_index: u16 },
    SetPropShaped { obj: u16, shape_id: u64, slot_index: u16, src: u16 },
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

const TAG_INT32: u64 = 0x7FF8_0001_0000_0000;

/// Why a function is not yet supported by the template baseline Tier 1 path.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TemplateCompileError {
    #[error("unsupported opcode at pc {pc}: {opcode:?}")]
    UnsupportedOpcode { pc: u32, opcode: Opcode },
    #[error("jump target out of range at pc {pc}: offset={offset}")]
    InvalidJumpTarget { pc: u32, offset: i32 },
    #[error("missing metadata for CallDirect at pc {pc}")]
    MissingCallMetadata { pc: u32 },
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
    #[error(
        "branch target out of range for template emission: from={source_offset} to pc={target_pc}"
    )]
    BranchTargetOutOfRange { source_offset: u32, target_pc: u32 },
}

/// Analyze whether a function can be compiled by the template baseline path.
///
/// The current supported subset is intentionally narrow:
/// `LoadI32`, `Move`, `Add`, `Sub`, `Mul`, `Lt`, `Jump`, `JumpIfFalse`, `Return`.
pub fn analyze_template_candidate(
    function: &Function,
    property_profile: &[Option<otter_vm::PropertyInlineCache>],
) -> Result<TemplateProgram, TemplateCompileError> {
    let instructions = function.bytecode().instructions();
    let mut lowered = Vec::with_capacity(instructions.len());
    let mut loop_headers = Vec::new();

    for (pc, instruction) in instructions.iter().enumerate() {
        let pc = pc as u32;
        lowered.push(lower_instruction(pc, *instruction, function, property_profile)?);

        match instruction.opcode() {
            Opcode::Jump | Opcode::JumpIfFalse => {
                let target_pc = resolve_target_pc(pc, instruction.immediate_i32()).ok_or(
                    TemplateCompileError::InvalidJumpTarget {
                        pc,
                        offset: instruction.immediate_i32(),
                    },
                )?;
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
        Err(TemplateEmitError::UnsupportedHostArch(
            std::env::consts::ARCH,
        ))
    }
}

#[cfg(target_arch = "aarch64")]
fn emit_template_stencil_aarch64(
    program: &TemplateProgram,
) -> Result<CodeBuffer, TemplateEmitError> {
    use crate::arch::aarch64::{Assembler, Cond, Reg};

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

    #[derive(Debug, Clone, Copy)]
    struct BailoutPatch {
        source_offset: u32,
        pc: u32,
        reason: crate::BailoutReason,
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
    let mut bailout_patches = Vec::new();

    // Push callee-saved x19 and link register.
    asm.push_x19_lr();
    // Save JitContext* (x0) into x19.
    asm.mov_rr(Reg::X19, Reg::X0);
    // Load registers_base once into x9.
    asm.ldr_u64_imm(Reg::X9, Reg::X19, 0);

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
            TemplateInstruction::CallDirect { dst, callee_fn_idx, arg_base, arg_count } => {
                // Prepare arguments across X0..X4 for C ABI.
                // X0 = ctx (from X19)
                asm.mov_rr(Reg::X0, Reg::X19);
                // X1 = callee_fn_idx
                asm.mov_imm64(Reg::X1, u64::from(*callee_fn_idx));
                // X2 = arg_base
                asm.mov_imm64(Reg::X2, u64::from(*arg_base));
                // X3 = arg_count
                asm.mov_imm64(Reg::X3, u64::from(*arg_count));
                // X4 = bytecode_pc
                asm.mov_imm64(Reg::X4, pc as u64);

                let helper_ptr = crate::runtime_helpers::otter_baseline_call_direct as *const () as u64;
                asm.mov_imm64(Reg::X10, helper_ptr);
                asm.blr(Reg::X10);

                // C function call might clobber X9. Reload registers_base.
                asm.ldr_u64_imm(Reg::X9, Reg::X19, 0);

                // Return value is in X0. Store it into `dst` slot.
                asm.str_u64_imm(Reg::X0, Reg::X9, slot_offset(*dst)?);
            }
            TemplateInstruction::Return { src } => {
                asm.ldr_u64_imm(Reg::X0, Reg::X9, slot_offset(*src)?);
                asm.pop_x19_lr();
                asm.ret();
            }
            TemplateInstruction::GetPropShaped { dst, obj, shape_id, slot_index } => {
                // 1. Load boxed object from slot
                asm.ldr_u64_imm(Reg::X10, Reg::X9, slot_offset(*obj)?);
                
                // 2. Check object tag
                asm.check_object_tag(Reg::X10);
                let jump_invalid_type = asm.b_cond_placeholder(Cond::Ne);
                
                // 3. Extract handle (lower 32 bits)
                asm.extract_int32(Reg::X10, Reg::X10);
                
                // 4. Resolve object pointer: slots_base + handle * 32
                asm.ldr_u64_imm(Reg::X11, Reg::X19, crate::context::offsets::HEAP_SLOTS_BASE as u32);
                asm.add_rrr_lsl(Reg::X11, Reg::X11, Reg::X10, 5);
                asm.ldr_u64_imm(Reg::X12, Reg::X11, 0); // Load Box<JsObject> data pointer
                
                // 5. Shape guard
                asm.ldr_u64_imm(Reg::X13, Reg::X12, crate::context::offsets::js_object::SHAPE_ID as u32);
                asm.mov_imm64(Reg::X14, *shape_id);
                asm.cmp_rr(Reg::X13, Reg::X14);
                let jump_shape_mismatch = asm.b_cond_placeholder(Cond::Ne);
                
                // 6. Load from values buffer: values.as_ptr() is at offset 40
                asm.ldr_u64_imm(Reg::X13, Reg::X12, crate::context::offsets::js_object::VALUES_PTR as u32);
                asm.ldr_u64_imm(Reg::X10, Reg::X13, u32::from(*slot_index) * 8);
                
                // 7. Store result
                asm.str_u64_imm(Reg::X10, Reg::X9, slot_offset(*dst)?);

                bailout_patches.push(BailoutPatch {
                    source_offset: jump_invalid_type,
                    pc: pc as u32,
                    reason: crate::BailoutReason::TypeGuardFailed,
                });
                bailout_patches.push(BailoutPatch {
                    source_offset: jump_shape_mismatch,
                    pc: pc as u32,
                    reason: crate::BailoutReason::ShapeGuardFailed,
                });

            }
            TemplateInstruction::SetPropShaped { obj, shape_id, slot_index, src } => {
                // 1. Load boxed object from slot
                asm.ldr_u64_imm(Reg::X10, Reg::X9, slot_offset(*obj)?);

                // 2. Check object tag
                asm.check_object_tag(Reg::X10);
                let jump_invalid_type = asm.b_cond_placeholder(Cond::Ne);

                // 3. Extract handle (lower 32 bits)
                asm.extract_int32(Reg::X10, Reg::X10);

                // 4. Resolve object pointer: slots_base + handle * 32
                asm.ldr_u64_imm(Reg::X11, Reg::X19, crate::context::offsets::HEAP_SLOTS_BASE as u32);
                asm.add_rrr_lsl(Reg::X11, Reg::X11, Reg::X10, 5);
                asm.ldr_u64_imm(Reg::X12, Reg::X11, 0); // Load Box<JsObject> data pointer

                // 5. Shape guard
                asm.ldr_u64_imm(Reg::X13, Reg::X12, crate::context::offsets::js_object::SHAPE_ID as u32);
                asm.mov_imm64(Reg::X14, *shape_id);
                asm.cmp_rr(Reg::X13, Reg::X14);
                let jump_shape_mismatch = asm.b_cond_placeholder(Cond::Ne);

                // 6. Load SRC value from its slot
                asm.ldr_u64_imm(Reg::X10, Reg::X9, slot_offset(*src)?);

                // 7. Write to values buffer: values.as_ptr() is at offset 40
                asm.ldr_u64_imm(Reg::X13, Reg::X12, crate::context::offsets::js_object::VALUES_PTR as u32);
                asm.str_u64_imm(Reg::X10, Reg::X13, u32::from(*slot_index) * 8);

                // 8. Call write barrier: otter_baseline_write_barrier(ctx, obj_handle_bits, value_bits)
                asm.mov_rr(Reg::X0, Reg::X19);
                asm.ldr_u64_imm(Reg::X1, Reg::X9, slot_offset(*obj)?);
                asm.mov_rr(Reg::X2, Reg::X10);

                let barrier_ptr = crate::runtime_helpers::otter_baseline_write_barrier as *const () as u64;
                asm.mov_imm64(Reg::X13, barrier_ptr);
                asm.blr(Reg::X13);

                // 9. Reload X9 (registers_base)
                asm.ldr_u64_imm(Reg::X9, Reg::X19, 0);

                // Bailout patches
                bailout_patches.push(BailoutPatch {
                    source_offset: jump_invalid_type,
                    pc: pc as u32,
                    reason: crate::BailoutReason::TypeGuardFailed,
                });
                bailout_patches.push(BailoutPatch {
                    source_offset: jump_shape_mismatch,
                    pc: pc as u32,
                    reason: crate::BailoutReason::ShapeGuardFailed,
                });
            }
        }

        pc += 1;
    }

    // Shared bailout block
    let bailout_common = asm.position();
    // X10 = pc, X11 = reason (set by instruction-specific bailout pads)
    asm.str_u64_imm(Reg::X10, Reg::X19, crate::context::offsets::BAILOUT_PC as u32);
    asm.str_u64_imm(Reg::X11, Reg::X19, crate::context::offsets::BAILOUT_REASON as u32);
    asm.mov_imm64(Reg::X0, crate::BAILOUT_SENTINEL);
    asm.pop_x19_lr();
    asm.ret();

    // Drop assembler to allow patching the buffer directly
    std::mem::drop(asm);

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

    for patch in bailout_patches {
        let pad_offset = buf.len() as u32;
        // Patch the guard to jump to this pad
        let delta = i64::from(pad_offset) - i64::from(patch.source_offset);
        let existing = buf.read_u32_le(patch.source_offset).unwrap();
        let imm19 = ((delta / 4) as i32 as u32) & 0x0007_FFFF;
        buf.patch_u32_le(patch.source_offset, existing | (imm19 << 5));

        // Create a new assembler for the pad
        let mut pad_asm = Assembler::new(&mut buf);
        // Pad sequence: set PC and Reason, then jump to common bailout
        pad_asm.mov_imm64(Reg::X10, u64::from(patch.pc));
        pad_asm.mov_imm64(Reg::X11, patch.reason as u64);
        let jump_common = pad_asm.b_placeholder();
        std::mem::drop(pad_asm); // Drop to patch

        let common_delta = i64::from(bailout_common) - i64::from(jump_common);
        let imm26 = ((common_delta / 4) as i32 as u32) & 0x03FF_FFFF;
        buf.patch_u32_le(jump_common, 0x14000000 | imm26);
    }

    Ok(buf)
}

fn lower_instruction(
    pc: u32,
    instruction: Instruction,
    function: &Function,
    profile: &[Option<otter_vm::PropertyInlineCache>],
) -> Result<TemplateInstruction, TemplateCompileError> {
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
            target_pc: resolve_target_pc(pc, instruction.immediate_i32()).ok_or(
                TemplateCompileError::InvalidJumpTarget {
                    pc,
                    offset: instruction.immediate_i32(),
                },
            )?,
        }),
        Opcode::JumpIfFalse => Ok(TemplateInstruction::JumpIfFalse {
            cond: instruction.a(),
            target_pc: resolve_target_pc(pc, instruction.immediate_i32()).ok_or(
                TemplateCompileError::InvalidJumpTarget {
                    pc,
                    offset: instruction.immediate_i32(),
                },
            )?,
        }),
        Opcode::Return => Ok(TemplateInstruction::Return {
            src: instruction.a(),
        }),
        Opcode::CallDirect => {
            let call = function.calls().get_direct(pc).ok_or(
                TemplateCompileError::MissingCallMetadata { pc }
            )?;
            Ok(TemplateInstruction::CallDirect {
                dst: instruction.a(),
                callee_fn_idx: call.callee().0,
                arg_base: instruction.b(),
                arg_count: call.argument_count(),
            })
        }
        Opcode::GetProperty => {
            if let Some(Some(cache)) = profile.get(pc as usize) {
                Ok(TemplateInstruction::GetPropShaped {
                    dst: instruction.a(),
                    obj: instruction.b(),
                    shape_id: cache.shape_id().0,
                    slot_index: cache.slot_index(),
                })
            } else {
                Err(TemplateCompileError::UnsupportedOpcode { pc, opcode: Opcode::GetProperty })
            }
        }
        Opcode::SetProperty => {
            if let Some(Some(cache)) = profile.get(pc as usize) {
                Ok(TemplateInstruction::SetPropShaped {
                    obj: instruction.a(),
                    shape_id: cache.shape_id().0,
                    slot_index: cache.slot_index(),
                    src: instruction.b(),
                })
            } else {
                Err(TemplateCompileError::UnsupportedOpcode { pc, opcode: Opcode::SetProperty })
            }
        }
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
        let program = analyze_template_candidate(&function, &[]).expect("subset should be accepted");

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

        let err = analyze_template_candidate(&function, &[]).expect_err("new_object is unsupported");
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
        let program = analyze_template_candidate(&function, &[]).expect("subset should be accepted");
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

        let program = analyze_template_candidate(&function, &[]).expect("subset should be accepted");
        let buf = emit_template_stencil(&program).expect("stencil emission should succeed");
        assert!(buf.len() >= 16, "const+return stencil should not be tiny");
        assert_eq!(buf.len() % 4, 0, "aarch64 instructions are fixed-width");
        assert_eq!(&buf.bytes()[buf.len() - 4..], &[0xC0, 0x03, 0x5F, 0xD6]);
    }
}
