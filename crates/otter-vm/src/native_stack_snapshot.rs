//! Source diagnostics from the one published JavaScript caller chain.
//!
//! # Contents
//! - Physical frame traversal and exact compiled source positions.
//! - Virtual inline parents from code-owned safepoint recipes.
//!
//! # Invariants
//! Each physical JavaScript activation is visited once; host frames carry no
//! source position and are skipped. Every activation's PC identifies its
//! current source operation: suspended compiled callers resolve the exact
//! child/request return site; active helpers publish their current operation.
//! Interpreter callers stand at the call instruction.
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
                source_position: frame.source.and_then(|source| {
                    let (line, column) = source.line_col(frame.span.0);
                    Some(crate::ErrorSourcePosition {
                        script_name: frame.module.to_owned(),
                        line_number: line,
                        start_column: column.saturating_sub(1),
                        source_line: source.line_source(line)?,
                    })
                }),
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
        // Resolve against the complete physical chain first. A Host child owns
        // its compiled caller's return anchor even though it has no JS source.
        let mut natives: Vec<_> = self.jit_native_frames().collect();
        natives.reverse();
        let mut sites = Vec::with_capacity(natives.len());
        for address in natives {
            let native = unsafe { &*address };
            if native.header.kind == crate::native_abi::NativeFrameKind::Host {
                continue;
            }
            let call = self
                .jit_frame_safepoint(native)
                .expect("published source anchor must resolve exactly");
            let pc = self.jit_frame_source_pc(native, call);
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
