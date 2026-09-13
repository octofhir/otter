//! Owned and borrowed views of the active JavaScript stack.
//!
//! Error reporting needs a complete owned stack, while bounded diagnostic
//! samplers must inspect frame metadata before allocating owned strings. This
//! module keeps both paths on one frame-resolution implementation.
//!
//! # Contents
//! - [`visit_frame_snapshot`] — resolve one exact source instruction.
//! - [`visit_frame_snapshots`] — visit materialized frames, innermost first.
//! - [`snapshot_frames`] — collect the materialized stack into owned DTOs.
//! - `native_stack_snapshot` merges compiled owners through this same resolver.
//!
//! # Invariants
//! - Foreign function ids are resolved through their owning execution context.
//! - A borrowed view lives only for its callback invocation; it never outlives
//!   an owned sibling context used to resolve that frame.
//! - Parent frames report the call-site instruction immediately before their
//!   already-advanced program counter.
//!
//! # See also
//! - [`crate::run_control::StackFrameSnapshot`]
//! - [`crate::cpu_profile`]

use crate::{ActivationStack, ExecutionContext, StackFrameSnapshot};

/// Borrowed metadata for one active JavaScript frame.
#[derive(Debug, Clone, Copy)]
pub(crate) struct StackFrameSnapshotView<'a> {
    /// VM-global function id.
    pub(crate) function_id: u32,
    /// Source-declared function name or the canonical unknown marker.
    pub(crate) function_name: &'a str,
    /// Per-function module URL, falling back to the owning module name.
    pub(crate) module: &'a str,
    /// Source byte span at the sampled instruction.
    pub(crate) span: (u32, u32),
}

/// Visit up to `limit` active frames, innermost first.
///
/// Returning `false` from `visit` stops the walk. The callback must not retain
/// borrowed strings after it returns.
pub(crate) fn visit_frame_snapshots(
    context: &ExecutionContext,
    stack: &ActivationStack,
    limit: usize,
    mut visit: impl FnMut(StackFrameSnapshotView<'_>) -> bool,
) {
    for (depth, frame) in stack.iter().rev().take(limit).enumerate() {
        let instruction = if depth == 0 {
            frame.pc
        } else {
            frame.pc.saturating_sub(1)
        };
        if !visit_frame_snapshot(context, frame.function_id, instruction as usize, &mut visit) {
            break;
        }
    }
}

/// Capture the complete active JavaScript stack into owned DTOs.
pub(crate) fn snapshot_frames(
    context: &ExecutionContext,
    stack: &ActivationStack,
) -> Vec<StackFrameSnapshot> {
    let mut frames = Vec::with_capacity(stack.len());
    visit_frame_snapshots(context, stack, usize::MAX, |frame| {
        frames.push(StackFrameSnapshot {
            function_id: frame.function_id,
            function_name: frame.function_name.to_owned(),
            module: frame.module.to_owned(),
            span: frame.span,
        });
        true
    });
    frames
}

/// Resolve one exact logical instruction into the shared borrowed stack view.
pub(crate) fn visit_frame_snapshot(
    context: &ExecutionContext,
    function_id: u32,
    instruction: usize,
    visit: impl FnOnce(StackFrameSnapshotView<'_>) -> bool,
) -> bool {
    let owner = context.for_function(function_id).ok();
    let owner = owner.as_deref();
    let function = owner.and_then(|owner| owner.function(function_id));
    let exec_function = owner.and_then(|owner| owner.exec_function(function_id));
    let function_name = function.map_or("<unknown>", |function| function.name.as_str());

    let byte_pc = exec_function
        .and_then(|function| function.instruction_byte_pc(instruction))
        .unwrap_or(0);
    let span = exec_function
        .and_then(|function| {
            let spans = function.byte_spans();
            let index = spans.partition_point(|span| span.pc <= byte_pc);
            if index == 0 {
                spans.first().map(|span| span.span)
            } else {
                Some(spans[index - 1].span)
            }
        })
        .or_else(|| function.map(|function| function.span))
        .unwrap_or((0, 0));
    let module = function
        .filter(|function| !function.module_url.is_empty())
        .map_or_else(
            || {
                owner
                    .map(ExecutionContext::module_name)
                    .unwrap_or_else(|| context.module_name())
            },
            |function| function.module_url.as_str(),
        );
    visit(StackFrameSnapshotView {
        function_id,
        function_name,
        module,
        span,
    })
}
