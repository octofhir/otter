//! x86 exact-source deoptimization and exception continuation encoding.
//!
//! # Contents
//! - Typed eager exits and local exception reconstruction targets.
//! - One canonical-home writeback call for full inline frame recovery.
//! - Shared return, parked-error and committed-throw continuations.
//!
//! # Invariants
//! - Recovery recipes read canonical homes or constants, never register dumps.
//! - A thrown value is rooted before a collecting writeback can move it.
//! - All C calls use the platform boundary; private materialization is aligned.
//!
//! # See also
//! - `super::super::frame` owns recipes and shared exit reasons.

use super::*;

impl<'a> Codegen<'a> {
    pub(super) fn eager_exit(&mut self, node: NodeId, reason: DeoptReason) -> DynamicLabel {
        let (reason, action) = exit_reason(reason);
        self.typed_exit(node, reason, action)
    }
    pub(super) fn typed_exit(
        &mut self,
        node: NodeId,
        reason: abi::ExitReason,
        action: abi::ExitAction,
    ) -> DynamicLabel {
        if let Some(site) = self.exits.iter().find(|site| {
            site.node == node && !site.lazy && site.reason == reason && site.action == action
        }) {
            return site.label;
        }
        let label = self.ops.new_dynamic_label();
        self.exits.push(ExitSite {
            label,
            node,
            lazy: false,
            reason,
            action,
        });
        label
    }
    pub(super) fn exit_index(
        &mut self,
        node: NodeId,
        reason: abi::ExitReason,
        action: abi::ExitAction,
    ) -> u32 {
        let label = self.ops.new_dynamic_label();
        self.exits.push(ExitSite {
            label,
            node,
            lazy: false,
            reason,
            action,
        });
        (self.exits.len() - 1) as u32
    }
    pub(super) fn in_exception_region(&self, pc: u32) -> bool {
        self.view.code_block.control_flow().handler_at(pc).is_some()
    }
    pub(super) fn throw_targets(&mut self, node: NodeId) -> (DynamicLabel, DynamicLabel) {
        self.throw_targets_at(node, self.graph.outer_pc(node))
    }
    pub(super) fn throw_targets_at(
        &mut self,
        node: NodeId,
        pc: u32,
    ) -> (DynamicLabel, DynamicLabel) {
        if !self.in_exception_region(pc) {
            return (self.threw, self.committed_throw);
        }
        let index = self.exit_index(
            node,
            abi::ExitReason::RuntimeTransition,
            abi::ExitAction::Resume,
        );
        let threw = self.ops.new_dynamic_label();
        let committed = self.ops.new_dynamic_label();
        let shared_threw = self.threw;
        let shared_committed = self.committed_throw;
        let materialize = self.materialize;
        self.deferred.push(Box::new(move |codegen| {
            dynasm!(codegen.ops ; .arch x64 ; =>threw);
            dynasm!(codegen.ops ; .arch x64 ; mov r11d, index as i32 ; call =>materialize ; jmp =>shared_threw ; =>committed);
            codegen.store_slot_gp(0, codegen.slots.exception_scratch());
            dynasm!(codegen.ops ; .arch x64 ; mov r11d, index as i32 ; call =>materialize);
            codegen.load_slot_gp(0, codegen.slots.exception_scratch());
            dynasm!(codegen.ops ; .arch x64 ; jmp =>shared_committed);
        }));
        (threw, committed)
    }
    pub(super) fn emit_return_path(&mut self) {
        let returned = self.returned;
        dynasm!(self.ops ; .arch x64 ; =>returned ; xor edx, edx);
        crate::x86_64::frame::emit_epilogue(
            &mut self.ops,
            self.activation,
            abi::NativeFrameKind::Optimizing,
            self.spill,
        );
    }
    pub(super) fn emit_exit_stubs(&mut self) {
        let fatal = self.fatal;
        dynasm!(self.ops ; .arch x64 ; =>fatal ; mov eax, tag::VALUE_UNDEFINED as i32 ; mov edx, abi::NativeResultStatus::Fatal as i32);
        crate::x86_64::frame::emit_epilogue(
            &mut self.ops,
            self.activation,
            abi::NativeFrameKind::Optimizing,
            self.spill,
        );
        self.emit_throw_handlers();
        self.emit_materialize();
        while let Some(deferred) = self.deferred.pop() {
            deferred(self);
        }
        let deopt = self.deopt;
        for index in 0..self.exits.len() {
            let site = self.exits[index].clone();
            dynasm!(self.ops ; .arch x64 ; =>site.label);
            let allocation = self.loc(site.node);
            let spills = if site.lazy {
                allocation.lazy_spills.clone()
            } else {
                allocation.eager_spills.clone()
            };
            self.emit_parallel_moves(spills);
            dynasm!(self.ops ; .arch x64 ; mov r11d, index as i32 ; jmp =>deopt);
        }
        self.emit_deopt_handler();
    }
    pub(super) fn emit_stub_call(&mut self, stub: abi::RuntimeStubDescriptor) {
        crate::x86_64::values::emit_load_runtime_stub(
            &mut self.ops,
            &mut self.relocations,
            self.transitions.entry(stub),
            stub,
        );
        crate::x86_64::call_abi::emit_runtime_call(&mut self.ops, stub);
    }
    pub(super) fn emit_throw_handlers(&mut self) {
        let (threw, committed, propagate) = (self.threw, self.committed_throw, self.propagate);
        let dispatch = self.ops.new_dynamic_label();
        let side_exit = self.activation.side_exit;
        let fatal = self.fatal;
        dynasm!(self.ops ; .arch x64 ; =>threw);
        // The body cannot resume: only the exception scratch stays rooted.
        self.emit_stamp(super::FIRST_SITE_SAFEPOINT);
        dynasm!(self.ops ; .arch x64 ; mov rdi, r15);
        crate::x86_64::values::emit_load_runtime_stub(
            &mut self.ops,
            &mut self.relocations,
            self.transitions.variadic_entry(abi::STUB_JIT_FINISH_ERROR),
            abi::STUB_JIT_FINISH_ERROR,
        );
        crate::x86_64::call_abi::emit_variadic_call(&mut self.ops, abi::STUB_JIT_FINISH_ERROR, 1);
        dynasm!(self.ops ; .arch x64 ; jmp =>dispatch ; =>committed);
        self.store_slot_gp(0, self.slots.exception_scratch());
        self.emit_stamp(super::FIRST_SITE_SAFEPOINT);
        dynasm!(self.ops ; .arch x64 ; mov rsi, rax ; mov rdi, r15);
        self.emit_stub_call(abi::STUB_JIT_ROUTE_THROW);
        dynasm!(self.ops ; .arch x64 ; =>dispatch
            ; cmp edx, abi::NativeResultStatus::SideExit as i32 ; je =>side_exit
            ; cmp edx, abi::NativeResultStatus::Throw as i32 ; jne =>fatal
            ; =>propagate ; mov edx, abi::NativeResultStatus::Throw as i32
        );
        crate::x86_64::frame::emit_epilogue(
            &mut self.ops,
            self.activation,
            abi::NativeFrameKind::Optimizing,
            self.spill,
        );
    }
    /// `r11d` is the exact recipe index. SysV-shaped prepared arguments are
    /// normalized by the C owner, including Microsoft's hidden pair pointer.
    pub(super) fn emit_writeback_call(&mut self, homes_delta: i32) {
        dynasm!(self.ops ; .arch x64 ; mov esi, r11d);
        crate::x86_64::frame::emit_publish_lazy_window(
            &mut self.ops,
            self.view.code_block.register_count,
        );
        dynasm!(self.ops ; .arch x64 ; mov rdi, r15 ; lea rcx, [rsp + homes_delta] ; mov r8, r13);
        emit_load_symbol_u64(
            &mut self.ops,
            &mut self.relocations,
            2,
            self.deopt_runtime,
            RelocationTarget::DeoptRuntimeData,
        );
        crate::x86_64::values::emit_load_runtime_stub(
            &mut self.ops,
            &mut self.relocations,
            self.transitions
                .variadic_entry(abi::STUB_JIT_DEOPT_WRITEBACK),
            abi::STUB_JIT_DEOPT_WRITEBACK,
        );
        crate::x86_64::call_abi::emit_variadic_call(
            &mut self.ops,
            abi::STUB_JIT_DEOPT_WRITEBACK,
            5,
        );
    }
    pub(super) fn emit_materialize(&mut self) {
        let materialize = self.materialize;
        let fatal = self.fatal;
        // Private call puts its return address below the canonical homes.
        dynasm!(self.ops ; .arch x64 ; =>materialize ; sub rsp, 8);
        self.emit_writeback_call(16);
        dynasm!(self.ops ; .arch x64 ; add rsp, 8
            ; cmp edx, abi::NativeResultStatus::SideExit as i32 ; jne =>fatal ; ret
        );
    }
    pub(super) fn emit_deopt_handler(&mut self) {
        let deopt = self.deopt;
        let side_exit = self.activation.side_exit;
        dynasm!(self.ops ; .arch x64 ; =>deopt);
        self.emit_writeback_call(0);
        dynasm!(self.ops ; .arch x64 ; cmp edx, abi::NativeResultStatus::SideExit as i32 ; je =>side_exit);
        crate::x86_64::frame::emit_epilogue(
            &mut self.ops,
            self.activation,
            abi::NativeFrameKind::Optimizing,
            self.spill,
        );
    }
}
