//! Source diagnostics from the one published JavaScript caller chain.
//!
//! # Contents
//! - Physical frame traversal and exact compiled source positions.
//! - Virtual inline parents from code-owned safepoint recipes.
//!
//! # Invariants
//! Each physical JavaScript activation is visited once; host frames carry no
//! source position and are skipped. Every activation's PC identifies its
//! current source operation: a compiled frame publishes it, and an
//! interpreter frame waiting on a call stands at the call instruction.
//! Inline parents have recipes rather than physical frames. The
//! walk reads scalar metadata without allocation in the GC heap or JS reentry.
//!
//! # See also
//! - [`crate::stack_snapshot`] for source resolution.
//! - [`crate::Interpreter::jit_native_frames`] for the published chain.

use crate::stack_snapshot::StackFrameSnapshotView;
use crate::{ExecutionContext, Interpreter, StackFrameSnapshot};

impl Interpreter {
    pub(crate) fn snapshot_active_frames(
        &self,
        context: &ExecutionContext,
        limit: usize,
    ) -> Vec<StackFrameSnapshot> {
        let mut result = Vec::new();
        self.visit_active_frame_snapshots(context, limit, |frame| {
            result.push(StackFrameSnapshot {
                function_id: frame.function_id,
                function_name: frame.function_name.to_owned(),
                module: frame.module.to_owned(),
                span: frame.span,
            });
            true
        });
        result
    }

    pub(crate) fn visit_active_frame_snapshots(
        &self,
        context: &ExecutionContext,
        limit: usize,
        mut visit: impl FnMut(StackFrameSnapshotView<'_>) -> bool,
    ) {
        // Host frames are physical boundaries for host bodies, not source
        // activations.
        let mut natives: Vec<_> = self
            .jit_native_frames()
            .filter(|&frame| {
                // SAFETY: the published chain remains live for this walk.
                let kind = unsafe { (*frame).header.kind };
                kind != crate::native_abi::NativeFrameKind::Host
            })
            .collect();
        natives.reverse();
        let mut sites = Vec::with_capacity(natives.len());
        let native_count = natives.len();
        for (position, address) in natives.into_iter().enumerate() {
            // SAFETY: the published chain remains live throughout this
            // metadata-only walk.
            let native = unsafe { &*address };
            let call = self
                .generated_call_record(native)
                .filter(|record| position + 1 < native_count || !record.inline_frames.is_empty());
            let pc = call
                .map(|record| record.call_pc)
                .filter(|&pc| pc != crate::native_abi::NO_CALL_PC)
                .unwrap_or(native.header.pc);
            sites.push((native.header.function_id, pc));
            if let Some(record) = call.filter(|record| !record.inline_frames.is_empty()) {
                push_virtual_inline_sites(context, record, &mut sites);
            }
        }
        for (function, pc) in sites.into_iter().rev().take(limit) {
            if !crate::stack_snapshot::visit_frame_snapshot(
                context,
                function,
                pc as usize,
                &mut visit,
            ) {
                break;
            }
        }
    }

    fn generated_call_record(
        &self,
        native: &crate::native_abi::Frame,
    ) -> Option<&crate::native_abi::SafepointRecord> {
        if native.call_site == crate::native_abi::NO_SAFEPOINT {
            return None;
        }
        self.jit_code_registry
            .safepoint_record(u64::from(native.code_object_id), native.call_site)
    }
}

fn push_virtual_inline_sites(
    context: &ExecutionContext,
    record: &crate::native_abi::SafepointRecord,
    sites: &mut Vec<(u32, u32)>,
) {
    for frame in &record.inline_frames {
        let Ok(owner) = context.for_function(frame.function_id) else {
            continue;
        };
        let Some(function) = owner.exec_function(frame.function_id) else {
            continue;
        };
        if let Some(pc) = (0..function.code.len())
            .find(|&index| function.instruction_byte_pc(index) == Some(frame.byte_pc))
            .and_then(|index| u32::try_from(index).ok())
        {
            sites.push((frame.function_id, pc));
        }
    }
}
