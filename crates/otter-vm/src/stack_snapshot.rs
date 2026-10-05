//! Source resolution for owned and borrowed JavaScript stack views.
//!
//! Error reporting needs a complete owned stack, while bounded diagnostic
//! samplers must inspect frame metadata before allocating owned strings. This
//! module keeps both paths on one frame-resolution implementation.
//!
//! # Contents
//! - [`visit_frame_snapshot`] — resolve one exact source instruction.
//! - `native_stack_snapshot` uses this source resolver for owned and borrowed walks.
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

use crate::ExecutionContext;

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
    /// Source retained by this exact owning code chunk, never an ambient URL map.
    pub(crate) source: Option<&'a crate::source_registry::ModuleSource>,
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
        source: owner.and_then(|owner| owner.source(module)),
    })
}
