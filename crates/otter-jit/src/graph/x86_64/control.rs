//! x86 Graph block control, phi edges and interrupt polling.
//!
//! # Contents
//! - Shared edge assignments followed by native jumps and predicates.
//! - Loop polling with canonical preservation and typed continuation routing.
//!
//! # Invariants
//! - Edge moves finish before a successor observes its assigned phi registers.
//! - Float comparisons keep ECMAScript unordered results, including `!=`.
//! - NoAlloc polls never expire tagged homes; recovery remains exact-source.
//!
//! # See also
//! - `super::super::emission` owns edge and forwarding decisions.

use super::*;

impl<'a> Codegen<'a> {
    pub(super) fn edge_moves(&self, from: BlockId, to: BlockId) -> Vec<Move> {
        super::super::emission::edge_moves(self.allocation, from, to)
    }
    pub(super) fn forwarded(&self, block: BlockId) -> BlockId {
        super::super::emission::forwarded(self.graph, self.allocation, block)
    }
    pub(super) fn emit_jump(&mut self, from: BlockId, to: BlockId, next: Option<BlockId>) {
        self.emit_parallel_moves(self.edge_moves(from, to));
        let to = self.forwarded(to);
        if next != Some(to) {
            let label = self.labels[&to];
            dynasm!(self.ops ; .arch x64 ; jmp =>label);
        }
    }
    pub(super) fn emit_control(
        &mut self,
        block: BlockId,
        node: NodeId,
        next: Option<BlockId>,
    ) -> Result<(), Unsupported> {
        let allocation = self.loc(node);
        match self.graph.node(node).kind {
            Kind::Jump(target) => self.emit_jump(block, target, next),
            Kind::JumpLoop(target) => {
                self.emit_backedge_poll(node)?;
                self.emit_parallel_moves(self.edge_moves(block, target));
                let label = self.labels[&target];
                dynasm!(self.ops ; .arch x64 ; jmp =>label);
            }
            Kind::Branch {
                kind,
                if_true,
                if_false,
            } => {
                let if_true = self.forwarded(if_true);
                let if_false = self.forwarded(if_false);
                let true_label = self.labels[&if_true];
                let false_label = self.labels[&if_false];
                match kind {
                    BranchKind::Int32(condition) => {
                        let a = Self::gp(allocation.inputs[0]);
                        let b = allocation.inputs[1];
                        self.emit_int32_compare(a, b);
                        if next == Some(if_true) {
                            self.emit_branch_condition(condition.negate(), false, false_label);
                            return Ok(());
                        }
                        self.emit_branch_condition(condition, false, true_label);
                    }
                    BranchKind::Float64(condition) => {
                        let a = Self::fp(allocation.inputs[0]);
                        let b = Self::fp(allocation.inputs[1]);
                        dynasm!(self.ops ; .arch x64 ; ucomisd Rx(a), Rx(b));
                        self.emit_branch_condition(condition, true, true_label);
                    }
                    BranchKind::TaggedEqual => {
                        let a = Self::gp(allocation.inputs[0]);
                        let b = Self::gp(allocation.inputs[1]);
                        dynasm!(self.ops ; .arch x64 ; cmp Rq(a), Rq(b) ; je =>true_label);
                    }
                    BranchKind::WordEqual(bits) => {
                        let a = Self::gp(allocation.inputs[0]);
                        self.load_immediate(10, u64::from(bits));
                        dynasm!(self.ops ; .arch x64 ; cmp Rq(a), r10 ; je =>true_label);
                    }
                    BranchKind::Nullish => {
                        let a = Self::gp(allocation.inputs[0]);
                        dynasm!(self.ops ; .arch x64
                            ; cmp Rq(a), tag::VALUE_NULL as i32 ; je =>true_label
                            ; cmp Rq(a), tag::VALUE_UNDEFINED as i32 ; je =>true_label
                        );
                    }
                    BranchKind::Truthy
                        if self
                            .graph
                            .node(self.graph.node(node).inputs[0])
                            .kind
                            .produces_boolean() =>
                    {
                        let a = Self::gp(allocation.inputs[0]);
                        dynasm!(self.ops ; .arch x64 ; cmp Rq(a), tag::VALUE_TRUE as i32 ; je =>true_label);
                    }
                    BranchKind::Truthy => {
                        let a = Self::gp(allocation.inputs[0]);
                        self.emit_truthy_branch(node, a, true_label, false_label);
                        return Ok(());
                    }
                }
                if next != Some(if_false) {
                    dynasm!(self.ops ; .arch x64 ; jmp =>false_label);
                }
            }
            Kind::Return => {
                self.emit_move(allocation.inputs[0], Location::Gp(0));
                if next.is_some() {
                    let returned = self.returned;
                    dynasm!(self.ops ; .arch x64 ; jmp =>returned);
                }
            }
            Kind::Deopt(reason) => {
                let label = self.eager_exit(node, reason);
                dynasm!(self.ops ; .arch x64 ; jmp =>label);
            }
            _ => return Err(Unsupported::OperandShape("graph x86 control")),
        }
        Ok(())
    }
    pub(super) fn emit_backedge_poll(&mut self, node: NodeId) -> Result<(), Unsupported> {
        let slow = self.ops.new_dynamic_label();
        let resume = self.ops.new_dynamic_label();
        dynasm!(self.ops ; .arch x64
            // An interrupt trip zeroes the countdown.
            ; mov r10, [r15 + crate::entry::THREAD_OFFSET as i32]
            ; mov r10, [r10 + crate::entry::VM_THREAD_BACKEDGE_FUEL_CELL_OFFSET as i32]
            ; sub QWORD [r10], 1 ; jle =>slow ; =>resume
        );
        let pc = self
            .graph
            .node(node)
            .eager
            .map(|state| self.graph.frame_state(state).pc)
            .unwrap_or(0);
        let exit = self.typed_exit(node, abi::ExitReason::Interrupt, abi::ExitAction::Resume);
        let (threw, _) = self.throw_targets_at(node, pc);
        let fatal = self.fatal;
        let poll_safepoint = self.helper_safepoint(node)?;
        self.deferred.push(Box::new(move |codegen| {
            let restore_resume = codegen.ops.new_dynamic_label();
            let restore_exit = codegen.ops.new_dynamic_label();
            let restore_threw = codegen.ops.new_dynamic_label();
            dynasm!(codegen.ops ; .arch x64 ; =>slow);
            codegen.save_live_homes(node);
            codegen.emit_stamp(poll_safepoint);
            dynasm!(codegen.ops ; .arch x64
                ; mov DWORD [r14 + crate::entry::NATIVE_FRAME_PC_OFFSET as i32], pc as i32
                ; mov rdi, r15
            );
            codegen.emit_stub_call(abi::STUB_JIT_BACKEDGE_POLL);
            dynasm!(codegen.ops ; .arch x64
                ; cmp eax, abi::NativeResultStatus::Success as i32 ; je =>restore_resume
                ; cmp eax, abi::NativeResultStatus::Yield as i32 ; je =>restore_resume
                ; cmp eax, abi::NativeResultStatus::Throw as i32 ; je =>restore_threw
                ; cmp eax, abi::NativeResultStatus::SideExit as i32 ; je =>restore_exit
                ; jmp =>fatal
            );
            for (label, target) in [
                (restore_resume, resume),
                (restore_exit, exit),
                (restore_threw, threw),
            ] {
                dynasm!(codegen.ops ; .arch x64 ; =>label);
                codegen.restore_live_homes(node);
                dynasm!(codegen.ops ; .arch x64 ; jmp =>target);
            }
        }));
        Ok(())
    }
}
