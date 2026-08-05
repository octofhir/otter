//! Inline tree over monomorphic bytecode call sites.
//!
//! The optimizing tier compiles one outermost function plus the callee bodies
//! it decides to splice into it. This module owns that decision and nothing
//! else: it turns the VM-baked monomorphic call-site candidates into a
//! cost-bounded, verified tree of frames, each naming the exact caller
//! instruction it replaces and the callee body that replaces it. Graph
//! construction, renaming, deopt frame chaining, and emission consume the tree;
//! none of them re-derive it.
//!
//! # Contents
//! - [`InlineId`] — dense identity of one frame; [`InlineId::ROOT`] is the
//!   compiled function itself.
//! - [`InlineFrame`] — one frame: its parent, the parent call site it replaces,
//!   and the callee body to build.
//! - [`InlineCost`] — one candidate's bytecode, depth, call, and loop cost.
//! - [`InlineTree`] — the whole budgeted decision, with [`InlineTree::verify`].
//!
//! # Invariants
//! - Frame ids are dense and ascending, and a frame's parent always precedes
//!   it, so a single forward walk sees every caller before its callees.
//! - The root frame has no parent and replaces no call site; every other frame
//!   has both.
//! - A frame's `call_pc` names a real `Op::Call` or `Op::CallMethodValue`
//!   instruction in its parent's body, and at most one frame claims a given
//!   (parent, call_pc).
//! - No function id repeats along a root-to-leaf path, so a recursive cycle can
//!   never be spliced into itself.
//! - Every accepted call fits the per-call budget and the sum of accepted costs
//!   fits the cumulative unit budget.
//! - Splicing is a speculation, not a proof: the emitter still guards the
//!   callee's identity at the call site and deoptimizes when it fails.
//!
//! # See also
//! - [`crate::ir::cfg`] — the graph built over this tree.
//! - `otter_vm::call_feedback` — the bounded call-target distribution the
//!   VM-side candidates are drawn from.

use std::sync::Arc;

use otter_bytecode::{Op, Operand};
use otter_vm::jit::JitMethodGuard;
use otter_vm::{CodeBlock, JitCompileSnapshot, JitInlineMethod, JitInstructionMetadata};

/// Maximum depth of spliced frames below the root.
pub const MAX_INLINE_DEPTH: u32 = 5;

/// Maximum weighted cost admitted at one monomorphic call site.
pub const MAX_INLINE_CALL_COST: u32 = 170;

/// Maximum weighted cost of every callee spliced into one optimized unit.
pub const MAX_INLINE_UNIT_COST: u32 = 920;

const INLINE_DEPTH_COST: u32 = 6;
const INLINE_LOOP_COST: u32 = 24;
const INLINE_NESTED_CALL_COST: u32 = 8;

/// Weighted bytecode cost of one accepted inline frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InlineCost {
    /// Number of bytecode instructions in the body.
    pub bytecodes: u32,
    /// Depth below the optimized unit's root.
    pub depth: u32,
    /// Number of call-like instructions left in the body.
    pub nested_calls: u32,
    /// Whether the body contains a backward control-flow edge.
    pub has_loop: bool,
    /// Weighted cost charged to the unit budget.
    pub total: u32,
}

/// Dense identity of one frame in an [`InlineTree`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InlineId(pub u32);

impl InlineId {
    /// The outermost compiled function.
    pub const ROOT: Self = Self(0);
}

/// The call site in a parent frame that a spliced frame replaces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlineCallSite {
    /// Frame owning the call instruction.
    pub parent: InlineId,
    /// Canonical logical PC of the call opcode in the parent's body.
    pub call_pc: u32,
    /// Parent register the call's result is written to.
    pub result_register: u16,
    /// Dynamic call shape and the parent value guarded before entry.
    pub kind: InlineCallKind,
    /// Parent registers holding the arguments, in formal-parameter order.
    pub argument_registers: Vec<u16>,
}

/// Dynamic call shape replaced by one spliced frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InlineCallKind {
    /// Ordinary call guarded by the callable's bytecode function identity.
    Plain {
        /// Parent register holding the callable value.
        callee_register: u16,
    },
    /// Method call guarded by receiver/prototype/method-slot identity.
    Method {
        /// Parent register holding the exact `this` receiver.
        receiver_register: u16,
        /// VM-baked immutable guard facts re-read by generated code.
        guard: JitMethodGuard,
    },
}

impl InlineCallSite {
    /// Parent register whose value becomes an inlined method frame's `this`.
    #[must_use]
    pub fn receiver_register(&self) -> Option<u16> {
        match self.kind {
            InlineCallKind::Plain { .. } => None,
            InlineCallKind::Method {
                receiver_register, ..
            } => Some(receiver_register),
        }
    }
}

/// Method-only slot facts retained with a spliced frame.
#[derive(Debug, Clone)]
pub struct InlineMethodData {
    /// Body byte PC to guarded value-slab byte offset.
    pub prop_offsets: rustc_hash::FxHashMap<u32, u32>,
    /// Non-receiver body byte PC to required object shape.
    pub prop_shapes: rustc_hash::FxHashMap<u32, u32>,
    /// Monomorphic nested method candidates keyed by body byte PC.
    pub nested_methods: rustc_hash::FxHashMap<u32, JitInlineMethod>,
}

/// One function body in the compiled unit.
///
/// A frame owns its whole compile input, not just its bytecode: constants,
/// global cells, property and element facts, and call plans are all resolved
/// against this body. Nothing in the unit may read another frame's tables —
/// a byte PC names an instruction only inside the body it came from.
#[derive(Debug, Clone)]
pub struct InlineFrame {
    /// Dense identity equal to this frame's index in [`InlineTree::frames`].
    pub id: InlineId,
    /// The call site this frame replaces; `None` only for [`InlineId::ROOT`].
    pub call_site: Option<InlineCallSite>,
    /// This frame's own baked compile inputs.
    pub body: Arc<JitCompileSnapshot>,
    /// Direct property facts for a method frame; absent for root/plain bodies.
    pub method: Option<InlineMethodData>,
    /// Weighted budget charge; zero only for the root frame.
    pub cost: u32,
}

impl InlineFrame {
    /// VM function id of this frame's body.
    #[must_use]
    pub fn function_id(&self) -> u32 {
        self.body.code_block.id
    }

    /// Authoritative executable body owning the operand side tables.
    #[must_use]
    pub fn code_block(&self) -> &CodeBlock {
        self.body.code_block.as_ref()
    }

    /// Instruction overlays in canonical logical-PC order.
    #[must_use]
    pub fn instructions(&self) -> &[JitInstructionMetadata] {
        &self.body.instructions
    }

    /// Depth below the root; the root itself is `0`.
    #[must_use]
    pub fn depth(&self, tree: &InlineTree) -> u32 {
        let mut depth = 0;
        let mut current = self;
        while let Some(call_site) = current.call_site.as_ref() {
            depth += 1;
            current = &tree.frames[call_site.parent.0 as usize];
        }
        depth
    }
}

/// Every function body compiled into one optimizing code object.
#[derive(Debug, Clone)]
pub struct InlineTree {
    /// Frames indexed by [`InlineId`]; index `0` is the root.
    pub frames: Vec<InlineFrame>,
}

/// A structural fault in a built or hand-written [`InlineTree`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InlineError {
    /// The tree has no root frame.
    Empty,
    /// A frame's id does not equal its index.
    IdMismatch {
        /// Index the frame occupies.
        index: u32,
        /// Identity the frame claims.
        id: InlineId,
    },
    /// The root frame claims a call site, or a non-root frame has none.
    RootShape {
        /// Offending frame.
        id: InlineId,
    },
    /// A frame's parent does not precede it.
    ParentOrder {
        /// Offending frame.
        id: InlineId,
        /// Parent it names.
        parent: InlineId,
    },
    /// A frame's `call_pc` is outside its parent's body.
    CallPcOutOfRange {
        /// Offending frame.
        id: InlineId,
        /// Parent PC it names.
        call_pc: u32,
    },
    /// A frame's `call_pc` does not name an `Op::Call`.
    CallSiteNotACall {
        /// Offending frame.
        id: InlineId,
        /// Parent PC it names.
        call_pc: u32,
    },
    /// Two frames claim the same parent call site.
    DuplicateCallSite {
        /// Frame owning the contested call.
        parent: InlineId,
        /// Contested PC.
        call_pc: u32,
    },
    /// The callee's formal arity disagrees with the spliced argument list.
    ArityMismatch {
        /// Offending frame.
        id: InlineId,
    },
    /// A function id repeats on a root-to-leaf path.
    RecursiveFrame {
        /// Offending frame.
        id: InlineId,
        /// Function id already on the path.
        function_id: u32,
    },
    /// A frame's stored budget charge does not match its body and depth.
    CostMismatch {
        /// Offending frame.
        id: InlineId,
        /// Charge stored with the frame.
        stored: u32,
        /// Charge recomputed by the verifier.
        computed: u32,
    },
    /// A frame is deeper than [`MAX_INLINE_DEPTH`].
    TooDeep {
        /// Offending frame.
        id: InlineId,
        /// Depth below the root.
        depth: u32,
    },
    /// One accepted frame exceeds the per-call cost ceiling.
    CallBudgetExceeded {
        /// Offending frame.
        id: InlineId,
        /// Recomputed weighted cost.
        cost: u32,
    },
    /// Accepted frames exceed the cumulative unit cost ceiling.
    UnitBudgetExceeded {
        /// Sum through the offending frame.
        cost: u32,
    },
    /// A frame's instruction overlays are not in canonical PC order.
    InstructionOrder {
        /// Offending frame.
        id: InlineId,
        /// Position in the overlay list.
        index: u32,
        /// PC the overlay resolves to.
        pc: u32,
    },
}

impl InlineTree {
    /// Decide the inline tree for one compile snapshot.
    ///
    /// Splices a call site only when the VM baked a monomorphic candidate for
    /// it, the callee's formal arity matches the call exactly (no argument
    /// padding or dropping is modelled), its weighted cost fits the per-call
    /// and cumulative budgets, and the callee does not already appear on the
    /// path. Everything else keeps its ordinary call.
    #[must_use]
    pub fn build(view: &JitCompileSnapshot) -> Self {
        let mut frames = Self::trivial(view).frames;
        let mut unit_cost = 0_u32;
        // Breadth-first over the frames already accepted, so `MAX_INLINE_DEPTH`
        // is enforced by construction and every parent precedes its callees.
        let mut next = 0usize;
        while next < frames.len() {
            let parent_id = InlineId(next as u32);
            let depth = frame_depth(&frames, parent_id);
            if depth >= MAX_INLINE_DEPTH {
                next += 1;
                continue;
            }
            let candidates = Self::candidates_in(view, &frames, parent_id);
            for candidate in candidates {
                let candidate_depth = depth + 1;
                let cost = inline_cost(&candidate.body, candidate_depth);
                let Some(next_unit_cost) = unit_cost.checked_add(cost.total) else {
                    continue;
                };
                if cost.total > MAX_INLINE_CALL_COST || next_unit_cost > MAX_INLINE_UNIT_COST {
                    continue;
                }
                let id = InlineId(frames.len() as u32);
                frames.push(InlineFrame {
                    id,
                    call_site: Some(candidate.call_site),
                    body: candidate.body,
                    method: candidate.method,
                    cost: cost.total,
                });
                unit_cost = next_unit_cost;
            }
            next += 1;
        }
        Self { frames }
    }

    /// Build the one-frame tree for `view`: nothing is spliced.
    #[must_use]
    pub fn trivial(view: &JitCompileSnapshot) -> Self {
        Self {
            frames: vec![InlineFrame {
                id: InlineId::ROOT,
                call_site: None,
                body: Arc::new(view.clone()),
                method: None,
                cost: 0,
            }],
        }
    }

    /// Collect the acceptable call sites of one already-accepted frame.
    ///
    /// The root reads candidates from the compile snapshot. An accepted method
    /// frame owns the recursively baked candidates for its byte-PC namespace,
    /// so deeper method calls never consult the root's maps by accident.
    fn candidates_in(
        view: &JitCompileSnapshot,
        frames: &[InlineFrame],
        parent_id: InlineId,
    ) -> Vec<InlineCandidate> {
        let parent = &frames[parent_id.0 as usize];
        let code_block = parent.code_block();
        let mut accepted = Vec::new();
        for instruction in parent.instructions() {
            match instruction.op(code_block) {
                Op::Call => {
                    if parent_id != InlineId::ROOT {
                        continue;
                    }
                    let Some(callee) = view.inline_callees.get(&instruction.byte_pc) else {
                        continue;
                    };
                    if Self::path_contains(frames, parent_id, callee.function_id()) {
                        continue;
                    }
                    let Some(call_site) =
                        Self::decode_call_site(parent_id, instruction, code_block)
                    else {
                        continue;
                    };
                    if call_site.argument_registers.len() != usize::from(callee.param_count()) {
                        continue;
                    }
                    accepted.push(InlineCandidate {
                        call_site,
                        body: Arc::clone(&callee.body),
                        method: None,
                    });
                }
                Op::CallMethodValue => {
                    let method = if parent_id == InlineId::ROOT {
                        view.inline_methods.get(&instruction.byte_pc)
                    } else {
                        parent
                            .method
                            .as_ref()
                            .and_then(|method| method.nested_methods.get(&instruction.byte_pc))
                    };
                    let Some(method) = method else {
                        continue;
                    };
                    if Self::path_contains(frames, parent_id, method.guard.method_fid) {
                        continue;
                    }
                    let Some(call_site) =
                        Self::decode_method_site(parent_id, instruction, code_block, &method.guard)
                    else {
                        continue;
                    };
                    if call_site.argument_registers.len() != usize::from(method.param_count()) {
                        continue;
                    }
                    accepted.push(InlineCandidate {
                        call_site,
                        body: Arc::clone(&method.body),
                        method: Some(InlineMethodData {
                            prop_offsets: method.prop_offsets.clone(),
                            prop_shapes: method.prop_shapes.clone(),
                            nested_methods: method.nested_methods.clone(),
                        }),
                    });
                }
                _ => {}
            }
        }
        accepted.sort_by_key(|candidate| candidate.call_site.call_pc);
        accepted
    }

    /// Decode `Op::Call`'s `dst, callee, argc, arguments…` operands.
    ///
    /// The schema declares the argument count at operand 2 and the argument
    /// registers as the variadic tail; a declared count that disagrees with the
    /// tail is a malformed instruction and declines the site.
    fn decode_call_site(
        parent: InlineId,
        instruction: &JitInstructionMetadata,
        code_block: &CodeBlock,
    ) -> Option<InlineCallSite> {
        const ARGUMENT_TAIL_START: usize = 3;
        let operands = instruction.operand_view(code_block);
        let register = |index: usize| match operands.get(index) {
            Some(otter_bytecode::Operand::Register(register)) => Some(register),
            _ => None,
        };
        let result_register = register(0)?;
        let callee_register = register(1)?;
        let otter_bytecode::Operand::ConstIndex(argc) = operands.get(2)? else {
            return None;
        };
        if operands.len() != ARGUMENT_TAIL_START + argc as usize {
            return None;
        }
        let mut argument_registers = Vec::with_capacity(argc as usize);
        for index in ARGUMENT_TAIL_START..operands.len() {
            argument_registers.push(register(index)?);
        }
        Some(InlineCallSite {
            parent,
            call_pc: instruction.instruction_pc(code_block),
            result_register,
            kind: InlineCallKind::Plain { callee_register },
            argument_registers,
        })
    }

    /// Decode `CallMethodValue`'s `dst, receiver, name, argc, arguments…`.
    fn decode_method_site(
        parent: InlineId,
        instruction: &JitInstructionMetadata,
        code_block: &CodeBlock,
        guard: &JitMethodGuard,
    ) -> Option<InlineCallSite> {
        const ARGUMENT_TAIL_START: usize = 4;
        let operands = instruction.operand_view(code_block);
        let register = |index: usize| match operands.get(index) {
            Some(otter_bytecode::Operand::Register(register)) => Some(register),
            _ => None,
        };
        let result_register = register(0)?;
        let receiver_register = register(1)?;
        let otter_bytecode::Operand::ConstIndex(argc) = operands.get(3)? else {
            return None;
        };
        if operands.len() != ARGUMENT_TAIL_START + argc as usize {
            return None;
        }
        let mut argument_registers = Vec::with_capacity(argc as usize);
        for index in ARGUMENT_TAIL_START..operands.len() {
            argument_registers.push(register(index)?);
        }
        Some(InlineCallSite {
            parent,
            call_pc: instruction.instruction_pc(code_block),
            result_register,
            kind: InlineCallKind::Method {
                receiver_register,
                guard: guard.clone(),
            },
            argument_registers,
        })
    }

    fn path_contains(frames: &[InlineFrame], from: InlineId, function_id: u32) -> bool {
        let mut current = Some(from);
        while let Some(id) = current {
            let frame = &frames[id.0 as usize];
            if frame.function_id() == function_id {
                return true;
            }
            current = frame.call_site.as_ref().map(|call_site| call_site.parent);
        }
        false
    }

    /// Independently re-check every structural invariant of the tree.
    ///
    /// This does not consult the feedback the tree was built from: it proves
    /// the tree is a well-formed, acyclic, bounded splice of real call sites,
    /// which is what every consumer relies on.
    pub fn verify(&self) -> Result<(), InlineError> {
        let Some(root) = self.frames.first() else {
            return Err(InlineError::Empty);
        };
        if root.call_site.is_some() {
            return Err(InlineError::RootShape { id: root.id });
        }
        let mut claimed = std::collections::BTreeSet::new();
        for (index, frame) in self.frames.iter().enumerate() {
            let index = index as u32;
            if frame.id != InlineId(index) {
                return Err(InlineError::IdMismatch {
                    index,
                    id: frame.id,
                });
            }
            for (position, instruction) in frame.instructions().iter().enumerate() {
                let pc = instruction.instruction_pc(frame.code_block());
                if pc != position as u32 {
                    return Err(InlineError::InstructionOrder {
                        id: frame.id,
                        index: position as u32,
                        pc,
                    });
                }
            }
            let Some(call_site) = frame.call_site.as_ref() else {
                if index != 0 {
                    return Err(InlineError::RootShape { id: frame.id });
                }
                if frame.cost != 0 {
                    return Err(InlineError::CostMismatch {
                        id: frame.id,
                        stored: frame.cost,
                        computed: 0,
                    });
                }
                continue;
            };
            if call_site.parent.0 >= index {
                return Err(InlineError::ParentOrder {
                    id: frame.id,
                    parent: call_site.parent,
                });
            }
            let parent = &self.frames[call_site.parent.0 as usize];
            let Some(instruction) = parent.instructions().get(call_site.call_pc as usize) else {
                return Err(InlineError::CallPcOutOfRange {
                    id: frame.id,
                    call_pc: call_site.call_pc,
                });
            };
            let expected_op = match call_site.kind {
                InlineCallKind::Plain { .. } => Op::Call,
                InlineCallKind::Method { .. } => Op::CallMethodValue,
            };
            if instruction.op(parent.code_block()) != expected_op {
                return Err(InlineError::CallSiteNotACall {
                    id: frame.id,
                    call_pc: call_site.call_pc,
                });
            }
            if !claimed.insert((call_site.parent, call_site.call_pc)) {
                return Err(InlineError::DuplicateCallSite {
                    parent: call_site.parent,
                    call_pc: call_site.call_pc,
                });
            }
            if call_site.argument_registers.len() != usize::from(frame.code_block().param_count) {
                return Err(InlineError::ArityMismatch { id: frame.id });
            }
            if matches!(call_site.kind, InlineCallKind::Method { .. }) != frame.method.is_some() {
                return Err(InlineError::CallSiteNotACall {
                    id: frame.id,
                    call_pc: call_site.call_pc,
                });
            }
            if Self::path_contains(&self.frames, call_site.parent, frame.function_id()) {
                return Err(InlineError::RecursiveFrame {
                    id: frame.id,
                    function_id: frame.function_id(),
                });
            }
            let depth = frame.depth(self);
            if depth > MAX_INLINE_DEPTH {
                return Err(InlineError::TooDeep {
                    id: frame.id,
                    depth,
                });
            }
            let computed = inline_cost(&frame.body, depth).total;
            if frame.cost != computed {
                return Err(InlineError::CostMismatch {
                    id: frame.id,
                    stored: frame.cost,
                    computed,
                });
            }
            if computed > MAX_INLINE_CALL_COST {
                return Err(InlineError::CallBudgetExceeded {
                    id: frame.id,
                    cost: computed,
                });
            }
        }
        let unit_cost = self.frames.iter().map(|frame| frame.cost).sum::<u32>();
        if unit_cost > MAX_INLINE_UNIT_COST {
            return Err(InlineError::UnitBudgetExceeded { cost: unit_cost });
        }
        Ok(())
    }

    /// `true` when nothing is spliced and the compiled unit is one function.
    #[must_use]
    pub fn is_trivial(&self) -> bool {
        self.frames.len() == 1
    }
}

struct InlineCandidate {
    call_site: InlineCallSite,
    body: Arc<JitCompileSnapshot>,
    method: Option<InlineMethodData>,
}

fn frame_depth(frames: &[InlineFrame], from: InlineId) -> u32 {
    let mut depth = 0;
    let mut current = &frames[from.0 as usize];
    while let Some(call_site) = current.call_site.as_ref() {
        depth += 1;
        current = &frames[call_site.parent.0 as usize];
    }
    depth
}

fn inline_cost(body: &JitCompileSnapshot, depth: u32) -> InlineCost {
    let code_block = body.code_block.as_ref();
    let instructions = &body.instructions;
    let bytecodes = u32::try_from(instructions.len()).unwrap_or(u32::MAX);
    let nested_calls = u32::try_from(
        instructions
            .iter()
            .filter(|instruction| {
                matches!(
                    instruction.op(code_block),
                    Op::Call | Op::CallMethodValue | Op::New
                )
            })
            .count(),
    )
    .unwrap_or(u32::MAX);
    let has_loop = instructions.iter().any(|instruction| {
        if !matches!(
            instruction.op(code_block),
            Op::Jump | Op::JumpIfFalse | Op::JumpIfTrue | Op::JumpIfNullish | Op::JumpViaFinally
        ) {
            return false;
        }
        let Some(Operand::Imm32(relative)) = instruction.operand_view(code_block).first() else {
            return false;
        };
        i64::from(instruction.instruction_pc(code_block)) + 1 + i64::from(relative)
            <= i64::from(instruction.instruction_pc(code_block))
    });
    let total = bytecodes
        .saturating_add(depth.saturating_mul(INLINE_DEPTH_COST))
        .saturating_add(nested_calls.saturating_mul(INLINE_NESTED_CALL_COST))
        .saturating_add(if has_loop { INLINE_LOOP_COST } else { 0 });
    InlineCost {
        bytecodes,
        depth,
        nested_calls,
        has_loop,
        total,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use otter_bytecode::Operand;
    use otter_vm::jit::JitTestInstruction;

    fn snapshot(fid: u32, instructions: Vec<JitTestInstruction>) -> JitCompileSnapshot {
        JitCompileSnapshot::without_feedback(fid, 0, 8, instructions)
    }

    fn callee(
        fid: u32,
        param_count: u16,
        instructions: Vec<JitTestInstruction>,
    ) -> otter_vm::JitInlineCallee {
        let view = JitCompileSnapshot::without_feedback(fid, param_count, 8, instructions);
        otter_vm::JitInlineCallee {
            body: Arc::new(view),
        }
    }

    fn method(
        fid: u32,
        param_count: u16,
        instructions: Vec<JitTestInstruction>,
        nested_methods: rustc_hash::FxHashMap<u32, JitInlineMethod>,
    ) -> JitInlineMethod {
        let view = JitCompileSnapshot::without_feedback(fid, param_count, 8, instructions);
        JitInlineMethod {
            body: Arc::new(view),
            guard: JitMethodGuard {
                method_fid: fid,
                recv_shape: fid + 100,
                proto_chain: Vec::new(),
                method_value_byte: 0,
            },
            prop_offsets: rustc_hash::FxHashMap::default(),
            prop_shapes: rustc_hash::FxHashMap::default(),
            nested_methods,
        }
    }

    /// `r0 = r1(r2)` at pc 0, then `return r0`.
    fn caller_with_one_call() -> Vec<JitTestInstruction> {
        vec![
            JitTestInstruction::new(
                Op::Call,
                0,
                0,
                vec![
                    Operand::Register(0),
                    Operand::Register(1),
                    Operand::ConstIndex(1),
                    Operand::Register(2),
                ],
            ),
            JitTestInstruction::new(Op::ReturnValue, 1, 8, vec![Operand::Register(0)]),
        ]
    }

    /// `r0 = r1.m(r2)` at pc 0, then `return r0`.
    fn caller_with_one_method_call() -> Vec<JitTestInstruction> {
        vec![
            JitTestInstruction::new(
                Op::CallMethodValue,
                0,
                0,
                vec![
                    Operand::Register(0),
                    Operand::Register(1),
                    Operand::ConstIndex(0),
                    Operand::ConstIndex(1),
                    Operand::Register(2),
                ],
            ),
            JitTestInstruction::new(Op::ReturnValue, 1, 10, vec![Operand::Register(0)]),
        ]
    }

    /// `return r0` — a one-parameter body.
    fn identity_body() -> Vec<JitTestInstruction> {
        vec![JitTestInstruction::new(
            Op::ReturnValue,
            0,
            0,
            vec![Operand::Register(0)],
        )]
    }

    #[test]
    fn no_candidates_yields_a_verified_trivial_tree() {
        let view = snapshot(7, caller_with_one_call());
        let tree = InlineTree::build(&view);

        assert!(tree.is_trivial());
        assert_eq!(tree.frames[0].id, InlineId::ROOT);
        assert_eq!(tree.frames[0].function_id(), 7);
        assert!(tree.frames[0].call_site.is_none());
        tree.verify().expect("a root-only tree is well formed");
    }

    #[test]
    fn monomorphic_candidate_splices_with_decoded_registers() {
        let mut view = snapshot(7, caller_with_one_call());
        let call_byte_pc = view.instructions[0].byte_pc;
        view.inline_callees
            .insert(call_byte_pc, callee(9, 1, identity_body()));

        let tree = InlineTree::build(&view);
        tree.verify().expect("a spliced tree is well formed");

        assert_eq!(tree.frames.len(), 2);
        let frame = &tree.frames[1];
        assert_eq!(frame.id, InlineId(1));
        assert_eq!(frame.function_id(), 9);
        let call_site = frame
            .call_site
            .as_ref()
            .expect("a spliced frame replaces a call");
        assert_eq!(call_site.parent, InlineId::ROOT);
        assert_eq!(call_site.call_pc, 0);
        assert_eq!(call_site.result_register, 0);
        assert_eq!(call_site.kind, InlineCallKind::Plain { callee_register: 1 });
        assert_eq!(call_site.argument_registers, vec![2_u16]);
        assert_eq!(frame.depth(&tree), 1);
    }

    #[test]
    fn arity_mismatch_keeps_the_ordinary_call() {
        let mut view = snapshot(7, caller_with_one_call());
        let call_byte_pc = view.instructions[0].byte_pc;
        // The call passes one argument; the body declares two formals.
        view.inline_callees
            .insert(call_byte_pc, callee(9, 2, identity_body()));

        let tree = InlineTree::build(&view);
        assert!(tree.is_trivial());
        tree.verify()
            .expect("declining to splice stays well formed");
    }

    #[test]
    fn over_budget_body_keeps_the_ordinary_call() {
        let mut view = snapshot(7, caller_with_one_call());
        let call_byte_pc = view.instructions[0].byte_pc;
        let mut body: Vec<JitTestInstruction> = (0..MAX_INLINE_CALL_COST + 1)
            .map(|pc| JitTestInstruction::new(Op::Nop, pc, pc * 8, Vec::new()))
            .collect();
        body.push(JitTestInstruction::new(
            Op::ReturnValue,
            MAX_INLINE_CALL_COST + 1,
            0,
            vec![Operand::Register(0)],
        ));
        view.inline_callees.insert(call_byte_pc, callee(9, 1, body));

        let tree = InlineTree::build(&view);
        assert!(tree.is_trivial());
    }

    #[test]
    fn self_recursive_candidate_is_never_spliced() {
        let mut view = snapshot(7, caller_with_one_call());
        let call_byte_pc = view.instructions[0].byte_pc;
        // The candidate is the compiled function itself.
        view.inline_callees
            .insert(call_byte_pc, callee(7, 1, identity_body()));

        let tree = InlineTree::build(&view);
        assert!(tree.is_trivial());
    }

    #[test]
    fn a_plain_nested_call_does_not_reuse_the_roots_byte_pc_map() {
        // The root calls 9, whose own body calls something at a byte PC that
        // also has a baked candidate. That candidate belongs to the root's byte
        // PC space, so the spliced frame must not consult it and 11 must not
        // appear in the tree.
        let mut view = snapshot(7, caller_with_one_call());
        let root_call_byte_pc = view.instructions[0].byte_pc;
        let nested = callee(9, 1, caller_with_one_call());
        assert_eq!(
            nested.body.instructions[0].byte_pc, root_call_byte_pc,
            "the fixture must reuse the byte PC to model the collision",
        );
        view.inline_callees.insert(root_call_byte_pc, nested);

        let tree = InlineTree::build(&view);
        tree.verify().expect("a depth-bounded tree is well formed");
        assert_eq!(tree.frames.len(), 2);
        assert_eq!(tree.frames[1].function_id(), 9);
        assert_eq!(tree.frames[1].depth(&tree), 1);
        assert_eq!(MAX_INLINE_DEPTH, 5);
    }

    #[test]
    fn recursively_baked_method_candidates_form_a_budgeted_chain() {
        let mut view = snapshot(7, caller_with_one_method_call());
        let leaf = method(11, 1, identity_body(), rustc_hash::FxHashMap::default());
        let mut nested_methods = rustc_hash::FxHashMap::default();
        nested_methods.insert(0, leaf);
        let middle = method(9, 1, caller_with_one_method_call(), nested_methods);
        view.inline_methods.insert(0, middle);

        let tree = InlineTree::build(&view);
        tree.verify()
            .expect("a recursively baked method chain is well formed");
        assert_eq!(tree.frames.len(), 3);
        assert_eq!(tree.frames[1].function_id(), 9);
        assert_eq!(tree.frames[1].depth(&tree), 1);
        assert_eq!(tree.frames[2].function_id(), 11);
        assert_eq!(tree.frames[2].depth(&tree), 2);
        assert!(tree.frames.iter().map(|frame| frame.cost).sum::<u32>() <= MAX_INLINE_UNIT_COST);
    }

    #[test]
    fn verify_rejects_a_call_site_that_is_not_a_call() {
        let mut view = snapshot(7, caller_with_one_call());
        let call_byte_pc = view.instructions[0].byte_pc;
        view.inline_callees
            .insert(call_byte_pc, callee(9, 1, identity_body()));
        let mut tree = InlineTree::build(&view);

        // Point the spliced frame at the `ReturnValue`, not the `Op::Call`.
        tree.frames[1].call_site.as_mut().expect("spliced").call_pc = 1;
        assert_eq!(
            tree.verify(),
            Err(InlineError::CallSiteNotACall {
                id: InlineId(1),
                call_pc: 1,
            })
        );
    }

    #[test]
    fn verify_rejects_a_parent_that_does_not_precede_its_frame() {
        let mut view = snapshot(7, caller_with_one_call());
        let call_byte_pc = view.instructions[0].byte_pc;
        view.inline_callees
            .insert(call_byte_pc, callee(9, 1, identity_body()));
        let mut tree = InlineTree::build(&view);

        tree.frames[1].call_site.as_mut().expect("spliced").parent = InlineId(1);
        assert_eq!(
            tree.verify(),
            Err(InlineError::ParentOrder {
                id: InlineId(1),
                parent: InlineId(1),
            })
        );
    }
}
