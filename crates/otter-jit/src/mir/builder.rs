//! VM bytecode -> MIR lowering for the early Tier 1 subset.

use std::collections::{BTreeSet, HashMap};

use otter_vm::PropertyInlineCache;
use otter_vm::bytecode::{BytecodeRegister, Instruction, Opcode};
use otter_vm::call::DirectCall;
use otter_vm::frame::{FrameLayout, RegisterIndex};
use otter_vm::module::Function;

use crate::JitError;
use crate::mir::graph::{BlockId, DeoptInfo, MirGraph, ResumeMode, ValueId};
use crate::mir::nodes::MirOp;
use crate::mir::types::CmpOp;

/// Build MIR from a VM function for the currently supported Tier 1 subset.
pub fn build_mir(
    function: &Function,
    property_profile: Option<&[Option<PropertyInlineCache>]>,
) -> Result<MirGraph, JitError> {
    let layout = function.frame_layout();
    let register_count = layout.register_count();
    let mut graph = MirGraph::new(
        function.name().unwrap_or("<anonymous>").to_string(),
        register_count,
        register_count,
        layout.parameter_count(),
    );
    let instructions = function.bytecode().instructions();
    let block_starts = find_block_starts(instructions);

    let mut pc_to_block = HashMap::new();
    for &pc in &block_starts {
        if pc == 0 {
            pc_to_block.insert(0_u32, graph.entry_block);
        } else {
            pc_to_block.insert(pc as u32, graph.create_block());
        }
    }

    let mut current_block = graph.entry_block;
    let mut reg_cache = RegisterValueCache::new();
    for (pc, instruction) in instructions.iter().enumerate() {
        let pc = pc as u32;
        if let Some(&block) = pc_to_block.get(&pc)
            && block != current_block
        {
            if !graph.block(current_block).is_terminated() {
                graph.push_instr(current_block, MirOp::Jump(block, vec![]), pc);
            }
            current_block = block;
            // Invalidate cache at block boundaries — predecessor values unknown.
            reg_cache.clear();
        }

        lower_instruction(
            &mut graph,
            function,
            current_block,
            pc,
            *instruction,
            property_profile,
            &pc_to_block,
            &mut reg_cache,
        )?;

        if graph.block(current_block).is_terminated() {
            current_block = match pc_to_block.get(&(pc + 1)) {
                Some(&next) => next,
                None => graph.create_block(),
            };
            // Invalidate cache at block boundaries.
            reg_cache.clear();
        }
    }

    graph.recompute_edges();
    Ok(graph)
}

fn find_block_starts(instructions: &[Instruction]) -> Vec<usize> {
    let mut starts = BTreeSet::new();
    starts.insert(0);
    for (pc, instruction) in instructions.iter().enumerate() {
        match instruction.opcode() {
            Opcode::Jump => {
                starts.insert(resolve_target_pc(pc as u32, instruction.immediate_i32()) as usize);
                starts.insert(pc + 1);
            }
            Opcode::JumpIfTrue | Opcode::JumpIfFalse => {
                starts.insert(resolve_target_pc(pc as u32, instruction.immediate_i32()) as usize);
                starts.insert(pc + 1);
            }
            Opcode::Return => {
                starts.insert(pc + 1);
            }
            _ => {}
        }
    }
    starts.into_iter().collect()
}

fn resolve_target_pc(pc: u32, offset: i32) -> u32 {
    let current = i64::from(pc);
    let target = current + 1 + i64::from(offset);
    u32::try_from(target).expect("jump target must fit into u32")
}

fn resolve_target_block(
    pc: u32,
    offset: i32,
    pc_to_block: &HashMap<u32, BlockId>,
) -> Result<BlockId, JitError> {
    let target_pc = resolve_target_pc(pc, offset);
    pc_to_block
        .get(&target_pc)
        .copied()
        .ok_or_else(|| JitError::Internal(format!("vm jump target pc={} is not mapped", target_pc)))
}

fn resolve_direct_call(function: &Function, pc: u32) -> Result<DirectCall, JitError> {
    function.calls().get_direct(pc).ok_or_else(|| {
        JitError::Internal(format!("vm direct call site at pc={} is not mapped", pc))
    })
}

fn create_deopt(graph: &mut MirGraph, pc: u32) -> crate::mir::graph::DeoptId {
    graph.create_deopt(DeoptInfo {
        bytecode_pc: pc,
        live_state: Vec::new(),
        resume_mode: ResumeMode::ResumeAtPc,
    })
}

fn resolve_register(layout: FrameLayout, raw: u16) -> Result<RegisterIndex, JitError> {
    layout
        .resolve_user_visible(raw)
        .ok_or_else(|| JitError::Internal(format!("vm register {} is out of bounds", raw)))
}

/// Tracks the MIR value last written to each register slot within
/// the current basic block. When a subsequent instruction reads
/// the same slot, we return the cached value directly instead of
/// emitting LoadLocal + GuardInt32 + UnboxInt32.
///
/// The cache is cleared at basic-block boundaries (jumps, branches)
/// because values from a different control-flow predecessor may differ.
struct RegisterValueCache {
    /// Maps `absolute_register_index → (boxed_value_id, Option<unboxed_i32_value_id>)`.
    /// The boxed value is the NaN-boxed representation.
    /// The optional i32 value is set when we know the boxed value is `BoxInt32(i32_val)`.
    cache: HashMap<u16, CachedRegValue>,
}

#[derive(Clone, Copy)]
struct CachedRegValue {
    /// The NaN-boxed value that was stored.
    boxed: ValueId,
    /// If known, the raw i32 value BEFORE boxing. Eliminates Guard+Unbox.
    raw_i32: Option<ValueId>,
}

impl RegisterValueCache {
    fn new() -> Self {
        Self {
            cache: HashMap::new(),
        }
    }

    fn clear(&mut self) {
        self.cache.clear();
    }
}

fn load_register(
    graph: &mut MirGraph,
    layout: FrameLayout,
    block: BlockId,
    pc: u32,
    register: BytecodeRegister,
) -> Result<ValueId, JitError> {
    let absolute = resolve_register(layout, register.index())?;
    Ok(graph.push_instr(block, MirOp::LoadLocal(absolute), pc))
}

/// Load a register with value forwarding: if the cache has a boxed value
/// and optionally a raw i32, return them directly without emitting LoadLocal.
fn load_register_cached(
    graph: &mut MirGraph,
    layout: FrameLayout,
    block: BlockId,
    pc: u32,
    register: BytecodeRegister,
    cache: &RegisterValueCache,
) -> Result<(ValueId, Option<ValueId>), JitError> {
    let absolute = resolve_register(layout, register.index())?;
    if let Some(entry) = cache.cache.get(&absolute) {
        return Ok((entry.boxed, entry.raw_i32));
    }
    let boxed = graph.push_instr(block, MirOp::LoadLocal(absolute), pc);
    Ok((boxed, None))
}

fn store_register(
    graph: &mut MirGraph,
    layout: FrameLayout,
    block: BlockId,
    pc: u32,
    register: BytecodeRegister,
    value: ValueId,
) -> Result<(), JitError> {
    let absolute = resolve_register(layout, register.index())?;
    graph.push_instr(
        block,
        MirOp::StoreLocal {
            idx: absolute,
            val: value,
        },
        pc,
    );
    Ok(())
}

fn store_register_cached(
    graph: &mut MirGraph,
    layout: FrameLayout,
    block: BlockId,
    pc: u32,
    register: BytecodeRegister,
    value: ValueId,
    cache: &mut RegisterValueCache,
    raw_i32: Option<ValueId>,
) -> Result<(), JitError> {
    let absolute = resolve_register(layout, register.index())?;
    graph.push_instr(
        block,
        MirOp::StoreLocal {
            idx: absolute,
            val: value,
        },
        pc,
    );
    cache.cache.insert(absolute, CachedRegValue { boxed: value, raw_i32 });
    Ok(())
}

fn lower_instruction(
    graph: &mut MirGraph,
    function: &Function,
    block: BlockId,
    pc: u32,
    instruction: Instruction,
    property_profile: Option<&[Option<PropertyInlineCache>]>,
    pc_to_block: &HashMap<u32, BlockId>,
    reg_cache: &mut RegisterValueCache,
) -> Result<(), JitError> {
    let layout = function.frame_layout();
    match instruction.opcode() {
        Opcode::Nop => {}
        Opcode::Move => {
            let value = load_register(
                graph,
                layout,
                block,
                pc,
                BytecodeRegister::new(instruction.b()),
            )?;
            store_register(
                graph,
                layout,
                block,
                pc,
                BytecodeRegister::new(instruction.a()),
                value,
            )?;
        }
        Opcode::LoadI32 => {
            let raw = graph.push_instr(block, MirOp::ConstInt32(instruction.immediate_i32()), pc);
            let boxed = graph.push_instr(block, MirOp::BoxInt32(raw), pc);
            store_register_cached(
                graph, layout, block, pc, BytecodeRegister::new(instruction.a()),
                boxed, reg_cache, Some(raw),
            )?;
        }
        Opcode::LoadTrue => {
            let value = graph.push_instr(block, MirOp::True, pc);
            store_register(
                graph,
                layout,
                block,
                pc,
                BytecodeRegister::new(instruction.a()),
                value,
            )?;
        }
        Opcode::LoadFalse => {
            let value = graph.push_instr(block, MirOp::False, pc);
            store_register(
                graph,
                layout,
                block,
                pc,
                BytecodeRegister::new(instruction.a()),
                value,
            )?;
        }
        Opcode::Add | Opcode::Sub | Opcode::Mul | Opcode::Div => {
            let (lhs_boxed, lhs_cached_i32) = load_register_cached(
                graph, layout, block, pc, BytecodeRegister::new(instruction.b()), reg_cache,
            )?;
            let (rhs_boxed, rhs_cached_i32) = load_register_cached(
                graph, layout, block, pc, BytecodeRegister::new(instruction.c()), reg_cache,
            )?;
            let deopt = create_deopt(graph, pc);
            // Skip GuardInt32 if we already know the value is i32 from the cache.
            let lhs_i32 = lhs_cached_i32.unwrap_or_else(|| {
                graph.push_instr(block, MirOp::GuardInt32 { val: lhs_boxed, deopt }, pc)
            });
            let rhs_i32 = rhs_cached_i32.unwrap_or_else(|| {
                graph.push_instr(block, MirOp::GuardInt32 { val: rhs_boxed, deopt }, pc)
            });
            let result = match instruction.opcode() {
                Opcode::Add => graph.push_instr(
                    block, MirOp::AddI32 { lhs: lhs_i32, rhs: rhs_i32, deopt }, pc,
                ),
                Opcode::Sub => graph.push_instr(
                    block, MirOp::SubI32 { lhs: lhs_i32, rhs: rhs_i32, deopt }, pc,
                ),
                Opcode::Mul => graph.push_instr(
                    block, MirOp::MulI32 { lhs: lhs_i32, rhs: rhs_i32, deopt }, pc,
                ),
                Opcode::Div => graph.push_instr(
                    block, MirOp::DivI32 { lhs: lhs_i32, rhs: rhs_i32, deopt }, pc,
                ),
                _ => unreachable!(),
            };
            let boxed = graph.push_instr(block, MirOp::BoxInt32(result), pc);
            store_register_cached(
                graph, layout, block, pc, BytecodeRegister::new(instruction.a()),
                boxed, reg_cache, Some(result),
            )?;
        }
        Opcode::Eq => {
            let lhs = load_register(
                graph,
                layout,
                block,
                pc,
                BytecodeRegister::new(instruction.b()),
            )?;
            let rhs = load_register(
                graph,
                layout,
                block,
                pc,
                BytecodeRegister::new(instruction.c()),
            )?;
            let cmp = graph.push_instr(block, MirOp::CmpStrictEq { lhs, rhs }, pc);
            let boxed = graph.push_instr(block, MirOp::BoxBool(cmp), pc);
            store_register(
                graph,
                layout,
                block,
                pc,
                BytecodeRegister::new(instruction.a()),
                boxed,
            )?;
        }
        Opcode::Lt => {
            let lhs = load_register(
                graph,
                layout,
                block,
                pc,
                BytecodeRegister::new(instruction.b()),
            )?;
            let rhs = load_register(
                graph,
                layout,
                block,
                pc,
                BytecodeRegister::new(instruction.c()),
            )?;
            let deopt = create_deopt(graph, pc);
            let lhs_i32 = graph.push_instr(block, MirOp::GuardInt32 { val: lhs, deopt }, pc);
            let rhs_i32 = graph.push_instr(block, MirOp::GuardInt32 { val: rhs, deopt }, pc);
            let cmp = graph.push_instr(
                block,
                MirOp::CmpI32 {
                    op: CmpOp::Lt,
                    lhs: lhs_i32,
                    rhs: rhs_i32,
                },
                pc,
            );
            let boxed = graph.push_instr(block, MirOp::BoxBool(cmp), pc);
            store_register(
                graph,
                layout,
                block,
                pc,
                BytecodeRegister::new(instruction.a()),
                boxed,
            )?;
        }
        Opcode::GetProperty => {
            let obj = load_register(
                graph,
                layout,
                block,
                pc,
                BytecodeRegister::new(instruction.b()),
            )?;
            let Some(cache) =
                property_profile.and_then(|profile| profile.get(pc as usize).copied().flatten())
            else {
                // No IC data — use generic property access via runtime helper call.
                // The property name index is in instruction.c().
                let prop_name_idx = u32::from(instruction.c());
                let value = graph.push_instr(
                    block,
                    MirOp::GetPropConstGeneric {
                        obj,
                        name_idx: prop_name_idx,
                        ic_index: 0,
                    },
                    pc,
                );
                store_register(
                    graph,
                    layout,
                    block,
                    pc,
                    BytecodeRegister::new(instruction.a()),
                    value,
                )?;
                return Ok(());
            };
            let deopt = create_deopt(graph, pc);
            let obj_ref = graph.push_instr(block, MirOp::GuardObject { val: obj, deopt }, pc);
            graph.push_instr(
                block,
                MirOp::GuardShape {
                    obj: obj_ref,
                    shape_id: cache.shape_id().0,
                    deopt,
                },
                pc,
            );
            let value = graph.push_instr(
                block,
                MirOp::GetPropShaped {
                    obj: obj_ref,
                    shape_id: cache.shape_id().0,
                    offset: u32::from(cache.slot_index()),
                    inline: cache.slot_index() < 8,
                },
                pc,
            );
            store_register(
                graph,
                layout,
                block,
                pc,
                BytecodeRegister::new(instruction.a()),
                value,
            )?;
        }
        Opcode::SetProperty => {
            let obj = load_register(
                graph,
                layout,
                block,
                pc,
                BytecodeRegister::new(instruction.a()),
            )?;
            let value = load_register(
                graph,
                layout,
                block,
                pc,
                BytecodeRegister::new(instruction.b()),
            )?;
            let Some(cache) =
                property_profile.and_then(|profile| profile.get(pc as usize).copied().flatten())
            else {
                // No IC data — use generic property set via runtime helper call.
                let prop_name_idx = u32::from(instruction.c());
                graph.push_instr(
                    block,
                    MirOp::SetPropConstGeneric {
                        obj,
                        name_idx: prop_name_idx,
                        val: value,
                        ic_index: 0,
                    },
                    pc,
                );
                return Ok(());
            };
            let deopt = create_deopt(graph, pc);
            let obj_ref = graph.push_instr(block, MirOp::GuardObject { val: obj, deopt }, pc);
            graph.push_instr(
                block,
                MirOp::GuardShape {
                    obj: obj_ref,
                    shape_id: cache.shape_id().0,
                    deopt,
                },
                pc,
            );
            graph.push_instr(
                block,
                MirOp::SetPropShaped {
                    obj: obj_ref,
                    shape_id: cache.shape_id().0,
                    offset: u32::from(cache.slot_index()),
                    val: value,
                    inline: cache.slot_index() < 8,
                },
                pc,
            );
        }
        Opcode::CallDirect => {
            let call = resolve_direct_call(function, pc)?;
            let target = graph.push_instr(block, MirOp::ConstInt32(call.callee().0 as i32), pc);
            let mut args = Vec::with_capacity(usize::from(call.argument_count()));
            for offset in 0..usize::from(call.argument_count()) {
                let offset = u16::try_from(offset).map_err(|_| {
                    JitError::Internal("vm direct call argument index overflow".to_string())
                })?;
                args.push(load_register(
                    graph,
                    layout,
                    block,
                    pc,
                    BytecodeRegister::new(instruction.b().saturating_add(offset)),
                )?);
            }
            let result = graph.push_instr(block, MirOp::CallDirect { target, args }, pc);
            store_register(
                graph,
                layout,
                block,
                pc,
                BytecodeRegister::new(instruction.a()),
                result,
            )?;
        }
        Opcode::Jump => {
            let target_pc = resolve_target_pc(pc, instruction.immediate_i32());
            if target_pc <= pc {
                graph.push_instr(block, MirOp::Safepoint { live: Vec::new() }, pc);
            }
            let target = resolve_target_block(pc, instruction.immediate_i32(), pc_to_block)?;
            graph.push_instr(block, MirOp::Jump(target, vec![]), pc);
        }
        Opcode::JumpIfTrue => {
            let cond = load_register(
                graph,
                layout,
                block,
                pc,
                BytecodeRegister::new(instruction.a()),
            )?;
            let truthy = graph.push_instr(block, MirOp::IsTruthy(cond), pc);
            let target_pc = resolve_target_pc(pc, instruction.immediate_i32());
            if target_pc <= pc {
                graph.push_instr(block, MirOp::Safepoint { live: Vec::new() }, pc);
            }
            let target = resolve_target_block(pc, instruction.immediate_i32(), pc_to_block)?;
            let fallthrough = resolve_target_block(pc, 0, pc_to_block)?;
            graph.push_instr(
                block,
                MirOp::Branch {
                    cond: truthy,
                    true_block: target,
                    true_args: vec![],
                    false_block: fallthrough,
                    false_args: vec![],
                },
                pc,
            );
        }
        Opcode::JumpIfFalse => {
            let cond = load_register(
                graph,
                layout,
                block,
                pc,
                BytecodeRegister::new(instruction.a()),
            )?;
            let truthy = graph.push_instr(block, MirOp::IsTruthy(cond), pc);
            let target_pc = resolve_target_pc(pc, instruction.immediate_i32());
            if target_pc <= pc {
                graph.push_instr(block, MirOp::Safepoint { live: Vec::new() }, pc);
            }
            let target = resolve_target_block(pc, instruction.immediate_i32(), pc_to_block)?;
            let fallthrough = resolve_target_block(pc, 0, pc_to_block)?;
            graph.push_instr(
                block,
                MirOp::Branch {
                    cond: truthy,
                    true_block: fallthrough,
                    true_args: vec![],
                    false_block: target,
                    false_args: vec![],
                },
                pc,
            );
        }
        Opcode::Return => {
            let value = load_register(
                graph,
                layout,
                block,
                pc,
                BytecodeRegister::new(instruction.a()),
            )?;
            graph.push_instr(block, MirOp::Return(value), pc);
        }
        // ---- LoadThis ----
        Opcode::LoadThis => {
            let value = graph.push_instr(block, MirOp::LoadThis, pc);
            store_register(
                graph,
                layout,
                block,
                pc,
                BytecodeRegister::new(instruction.a()),
                value,
            )?;
        }
        // ---- Bitwise operations (always i32 result) ----
        Opcode::BitAnd | Opcode::BitOr | Opcode::BitXor => {
            let (lhs, lhs_i32_cached) = load_register_cached(
                graph, layout, block, pc, BytecodeRegister::new(instruction.b()), reg_cache,
            )?;
            let (rhs, rhs_i32_cached) = load_register_cached(
                graph, layout, block, pc, BytecodeRegister::new(instruction.c()), reg_cache,
            )?;
            let deopt = create_deopt(graph, pc);
            let lhs_i32 = lhs_i32_cached.unwrap_or_else(|| {
                graph.push_instr(block, MirOp::GuardInt32 { val: lhs, deopt }, pc)
            });
            let rhs_i32 = rhs_i32_cached.unwrap_or_else(|| {
                graph.push_instr(block, MirOp::GuardInt32 { val: rhs, deopt }, pc)
            });
            let result = match instruction.opcode() {
                Opcode::BitAnd => {
                    graph.push_instr(block, MirOp::BitAnd { lhs: lhs_i32, rhs: rhs_i32 }, pc)
                }
                Opcode::BitOr => {
                    graph.push_instr(block, MirOp::BitOr { lhs: lhs_i32, rhs: rhs_i32 }, pc)
                }
                Opcode::BitXor => {
                    graph.push_instr(block, MirOp::BitXor { lhs: lhs_i32, rhs: rhs_i32 }, pc)
                }
                _ => unreachable!(),
            };
            let boxed = graph.push_instr(block, MirOp::BoxInt32(result), pc);
            store_register_cached(
                graph, layout, block, pc, BytecodeRegister::new(instruction.a()),
                boxed, reg_cache, Some(result),
            )?;
        }
        Opcode::Shl | Opcode::Shr | Opcode::UShr => {
            let (lhs, lhs_i32_cached) = load_register_cached(
                graph, layout, block, pc, BytecodeRegister::new(instruction.b()), reg_cache,
            )?;
            let (rhs, rhs_i32_cached) = load_register_cached(
                graph, layout, block, pc, BytecodeRegister::new(instruction.c()), reg_cache,
            )?;
            let deopt = create_deopt(graph, pc);
            let lhs_i32 = lhs_i32_cached.unwrap_or_else(|| {
                graph.push_instr(block, MirOp::GuardInt32 { val: lhs, deopt }, pc)
            });
            let rhs_i32 = rhs_i32_cached.unwrap_or_else(|| {
                graph.push_instr(block, MirOp::GuardInt32 { val: rhs, deopt }, pc)
            });
            let result = match instruction.opcode() {
                Opcode::Shl => {
                    graph.push_instr(block, MirOp::Shl { lhs: lhs_i32, rhs: rhs_i32 }, pc)
                }
                Opcode::Shr => {
                    graph.push_instr(block, MirOp::Shr { lhs: lhs_i32, rhs: rhs_i32 }, pc)
                }
                Opcode::UShr => {
                    graph.push_instr(block, MirOp::Ushr { lhs: lhs_i32, rhs: rhs_i32 }, pc)
                }
                _ => unreachable!(),
            };
            let boxed = graph.push_instr(block, MirOp::BoxInt32(result), pc);
            store_register_cached(
                graph, layout, block, pc, BytecodeRegister::new(instruction.a()),
                boxed, reg_cache, Some(result),
            )?;
        }
        // ---- Comparison ops (Gt, Gte, Lte) — same pattern as Lt ----
        Opcode::Gt => {
            let lhs = load_register(
                graph,
                layout,
                block,
                pc,
                BytecodeRegister::new(instruction.b()),
            )?;
            let rhs = load_register(
                graph,
                layout,
                block,
                pc,
                BytecodeRegister::new(instruction.c()),
            )?;
            let deopt = create_deopt(graph, pc);
            let lhs_i32 = graph.push_instr(block, MirOp::GuardInt32 { val: lhs, deopt }, pc);
            let rhs_i32 = graph.push_instr(block, MirOp::GuardInt32 { val: rhs, deopt }, pc);
            let cmp = graph.push_instr(
                block,
                MirOp::CmpI32 {
                    op: CmpOp::Gt,
                    lhs: lhs_i32,
                    rhs: rhs_i32,
                },
                pc,
            );
            let boxed = graph.push_instr(block, MirOp::BoxBool(cmp), pc);
            store_register(
                graph,
                layout,
                block,
                pc,
                BytecodeRegister::new(instruction.a()),
                boxed,
            )?;
        }
        Opcode::Gte => {
            let lhs = load_register(
                graph,
                layout,
                block,
                pc,
                BytecodeRegister::new(instruction.b()),
            )?;
            let rhs = load_register(
                graph,
                layout,
                block,
                pc,
                BytecodeRegister::new(instruction.c()),
            )?;
            let deopt = create_deopt(graph, pc);
            let lhs_i32 = graph.push_instr(block, MirOp::GuardInt32 { val: lhs, deopt }, pc);
            let rhs_i32 = graph.push_instr(block, MirOp::GuardInt32 { val: rhs, deopt }, pc);
            let cmp = graph.push_instr(
                block,
                MirOp::CmpI32 {
                    op: CmpOp::Ge,
                    lhs: lhs_i32,
                    rhs: rhs_i32,
                },
                pc,
            );
            let boxed = graph.push_instr(block, MirOp::BoxBool(cmp), pc);
            store_register(
                graph,
                layout,
                block,
                pc,
                BytecodeRegister::new(instruction.a()),
                boxed,
            )?;
        }
        Opcode::Lte => {
            let lhs = load_register(
                graph,
                layout,
                block,
                pc,
                BytecodeRegister::new(instruction.b()),
            )?;
            let rhs = load_register(
                graph,
                layout,
                block,
                pc,
                BytecodeRegister::new(instruction.c()),
            )?;
            let deopt = create_deopt(graph, pc);
            let lhs_i32 = graph.push_instr(block, MirOp::GuardInt32 { val: lhs, deopt }, pc);
            let rhs_i32 = graph.push_instr(block, MirOp::GuardInt32 { val: rhs, deopt }, pc);
            let cmp = graph.push_instr(
                block,
                MirOp::CmpI32 {
                    op: CmpOp::Le,
                    lhs: lhs_i32,
                    rhs: rhs_i32,
                },
                pc,
            );
            let boxed = graph.push_instr(block, MirOp::BoxBool(cmp), pc);
            store_register(
                graph,
                layout,
                block,
                pc,
                BytecodeRegister::new(instruction.a()),
                boxed,
            )?;
        }
        // ---- Mod (integer remainder) ----
        Opcode::Mod => {
            let lhs = load_register(
                graph,
                layout,
                block,
                pc,
                BytecodeRegister::new(instruction.b()),
            )?;
            let rhs = load_register(
                graph,
                layout,
                block,
                pc,
                BytecodeRegister::new(instruction.c()),
            )?;
            let deopt = create_deopt(graph, pc);
            let lhs_i32 = graph.push_instr(block, MirOp::GuardInt32 { val: lhs, deopt }, pc);
            let rhs_i32 = graph.push_instr(block, MirOp::GuardInt32 { val: rhs, deopt }, pc);
            let result = graph.push_instr(
                block,
                MirOp::ModI32 {
                    lhs: lhs_i32,
                    rhs: rhs_i32,
                    deopt,
                },
                pc,
            );
            let boxed = graph.push_instr(block, MirOp::BoxInt32(result), pc);
            store_register(
                graph,
                layout,
                block,
                pc,
                BytecodeRegister::new(instruction.a()),
                boxed,
            )?;
        }
        // ---- Simple loads ----
        Opcode::LoadUndefined => {
            let value = graph.push_instr(block, MirOp::Undefined, pc);
            store_register(
                graph,
                layout,
                block,
                pc,
                BytecodeRegister::new(instruction.a()),
                value,
            )?;
        }
        Opcode::LoadNull => {
            let value = graph.push_instr(block, MirOp::Null, pc);
            store_register(
                graph,
                layout,
                block,
                pc,
                BytecodeRegister::new(instruction.a()),
                value,
            )?;
        }
        Opcode::LoadF64 => {
            let float_id = otter_vm::float::FloatId(instruction.b());
            if let Some(value) = function.float_constants().get(float_id) {
                let raw = graph.push_instr(block, MirOp::ConstFloat64(value), pc);
                let boxed = graph.push_instr(block, MirOp::BoxFloat64(raw), pc);
                store_register(
                    graph,
                    layout,
                    block,
                    pc,
                    BytecodeRegister::new(instruction.a()),
                    boxed,
                )?;
            } else {
                let deopt = create_deopt(graph, pc);
                graph.push_instr(block, MirOp::Deopt(deopt), pc);
            }
        }
        // ---- LoadHole: TDZ sentinel for `let`/`const` variables ----
        Opcode::LoadHole => {
            // Hole is a special NaN-boxed tag value: 0x7FF8_0000_0000_0004
            let value = graph.push_instr(block, MirOp::Const(0x7FF8_0000_0000_0004), pc);
            store_register(
                graph,
                layout,
                block,
                pc,
                BytecodeRegister::new(instruction.a()),
                value,
            )?;
        }
        // ---- ToNumber: when value is already i32/f64, pass through; otherwise deopt ----
        Opcode::ToNumber => {
            let val = load_register(
                graph,
                layout,
                block,
                pc,
                BytecodeRegister::new(instruction.b()),
            )?;
            // For i32 values, ToNumber is a no-op — just pass through the boxed value.
            // If the guard fails (value is not int32), deopt to interpreter.
            let deopt = create_deopt(graph, pc);
            let _i32_val = graph.push_instr(block, MirOp::GuardInt32 { val, deopt }, pc);
            // Pass through the original boxed value (ToNumber on an int is identity)
            store_register(
                graph,
                layout,
                block,
                pc,
                BytecodeRegister::new(instruction.a()),
                val,
            )?;
        }
        // ---- Boolean negation ----
        Opcode::Not => {
            let val = load_register(
                graph,
                layout,
                block,
                pc,
                BytecodeRegister::new(instruction.b()),
            )?;
            let truthy = graph.push_instr(block, MirOp::IsTruthy(val), pc);
            let negated = graph.push_instr(block, MirOp::LogicalNot(truthy), pc);
            let boxed = graph.push_instr(block, MirOp::BoxBool(negated), pc);
            store_register(
                graph,
                layout,
                block,
                pc,
                BytecodeRegister::new(instruction.a()),
                boxed,
            )?;
        }
        _ => {
            let deopt = create_deopt(graph, pc);
            graph.push_instr(block, MirOp::Deopt(deopt), pc);
        }
    }

    Ok(())
}
