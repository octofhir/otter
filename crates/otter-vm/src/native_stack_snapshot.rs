//! Source diagnostics from the one published JavaScript caller chain.
//!
//! # Contents
//! - [`FrameSite`] — one raw, unresolved activation site.
//! - [`ThrowProvenance`] — where an in-flight exception was thrown, raw until
//!   the exception leaves its dispatch region or is reported.
//! - [`Interpreter::capture_active_sites`] — scalar walk of the published
//!   chain, innermost first, with no source resolution.
//! - [`resolve_frame_sites`] — owned snapshots from previously captured sites.
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
//! Captured sites name function ids and code positions only; they are
//! resolved before any code they name can be reclaimed: a throw settles them
//! into frames when it leaves its dispatch region, and every report of an
//! uncaught exception resolves them before its run returns.
//!
//! # See also
//! - [`crate::stack_snapshot`] for source resolution.
//! - [`crate::Interpreter::jit_native_frames`] for the published chain.

use crate::stack_snapshot::StackFrameSnapshotView;
use crate::{ExecutionContext, Interpreter, StackFrameSnapshot};

/// One unresolved activation site from the published caller chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FrameSite {
    /// A physical activation standing at an instruction index.
    Instruction { function_id: u32, pc: u32 },
    /// A virtual inline parent recorded by a code-owned byte PC.
    Inline { function_id: u32, byte_pc: u32 },
}

impl FrameSite {
    /// The `(function_id, instruction)` pair this site names, or `None` when
    /// an inline recipe's byte PC no longer lands on an instruction start.
    fn instruction(self, context: &ExecutionContext) -> Option<(u32, u32)> {
        match self {
            Self::Instruction { function_id, pc } => Some((function_id, pc)),
            Self::Inline {
                function_id,
                byte_pc,
            } => {
                let owner = context.for_function(function_id).ok()?;
                let function = owner.exec_function(function_id)?;
                (0..function.code.len())
                    .find(|&index| function.instruction_byte_pc(index) == Some(byte_pc))
                    .and_then(|index| u32::try_from(index).ok())
                    .map(|pc| (function_id, pc))
            }
        }
    }
}

/// Where an in-flight exception was thrown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ThrowProvenance {
    /// Raw sites of the activation chain at the throw, innermost first.
    Sites(Vec<FrameSite>),
    /// Frames resolved when the exception left a dispatch region, or carried
    /// by a completed run's failure.
    Frames(Vec<StackFrameSnapshot>),
}

impl Interpreter {
    /// Record the site of a new exception, unless a nested exception that is
    /// still propagating already owns the provenance.
    pub(crate) fn record_throw_site(&mut self) {
        if self.pending_throw_provenance.is_none() {
            self.pending_throw_provenance = Some(ThrowProvenance::Sites(self.capture_active_sites()));
        }
    }

    /// Replace the pending provenance with already resolved frames.
    pub(crate) fn set_uncaught_frames(&mut self, frames: Vec<StackFrameSnapshot>) {
        self.pending_throw_provenance = Some(ThrowProvenance::Frames(frames));
    }

    /// Forget the pending provenance: a handler absorbed its exception.
    pub(crate) fn clear_throw_provenance(&mut self) {
        self.pending_throw_provenance = None;
    }

    /// Resolve pending raw sites into frames while the code they name is live.
    pub(crate) fn settle_throw_provenance(&mut self, context: &ExecutionContext) {
        if let Some(ThrowProvenance::Sites(sites)) = &self.pending_throw_provenance {
            let frames = resolve_frame_sites(context, sites, usize::MAX);
            self.pending_throw_provenance = Some(ThrowProvenance::Frames(frames));
        }
    }

    /// The pending provenance as owned frames, leaving it in place.
    pub(crate) fn pending_uncaught_frames(&self) -> Vec<StackFrameSnapshot> {
        match &self.pending_throw_provenance {
            Some(ThrowProvenance::Sites(sites)) => self.resolve_sites_in_code_space(sites),
            Some(ThrowProvenance::Frames(frames)) => frames.clone(),
            None => Vec::new(),
        }
    }

    /// Take the pending provenance as owned frames.
    pub(crate) fn take_uncaught_frames(&mut self) -> Vec<StackFrameSnapshot> {
        match self.pending_throw_provenance.take() {
            Some(ThrowProvenance::Sites(sites)) => self.resolve_sites_in_code_space(&sites),
            Some(ThrowProvenance::Frames(frames)) => frames,
            None => Vec::new(),
        }
    }

    /// Take the pending provenance, or snapshot the live chain when no throw
    /// recorded one (a failure raised outside any throw site).
    pub(crate) fn take_uncaught_frames_or_snapshot(
        &mut self,
        context: Option<&ExecutionContext>,
    ) -> Vec<StackFrameSnapshot> {
        match (&self.pending_throw_provenance, context) {
            (None, Some(context)) => self.snapshot_active_frames(context, usize::MAX),
            _ => self.take_uncaught_frames(),
        }
    }

    /// Resolved frames pending for the in-flight exception, if any.
    #[cfg(test)]
    pub(crate) fn pending_frames_for_test(&self) -> Option<&Vec<StackFrameSnapshot>> {
        match &self.pending_throw_provenance {
            Some(ThrowProvenance::Frames(frames)) => Some(frames),
            _ => None,
        }
    }

    /// Resolve sites through the VM's code space, each against the exact live
    /// chunk that owns its function; a site whose chunk is gone is skipped.
    fn resolve_sites_in_code_space(&self, sites: &[FrameSite]) -> Vec<StackFrameSnapshot> {
        let mut frames = Vec::with_capacity(sites.len());
        for site in sites {
            let function_id = match *site {
                FrameSite::Instruction { function_id, .. } | FrameSite::Inline { function_id, .. } => {
                    function_id
                }
            };
            let Ok(owner) = ExecutionContext::for_function_in(&self.code_space, function_id) else {
                continue;
            };
            frames.extend(resolve_frame_sites(&owner, std::slice::from_ref(site), 1));
        }
        frames
    }

    pub(crate) fn snapshot_active_frames(
        &self,
        context: &ExecutionContext,
        limit: usize,
    ) -> Vec<StackFrameSnapshot> {
        resolve_frame_sites(context, &self.capture_active_sites(), limit)
    }

    /// Raw sites of every published JavaScript activation, innermost first.
    ///
    /// Reads only frame headers and safepoint records: no names, modules,
    /// source positions or allocations beyond the returned vector.
    pub(crate) fn capture_active_sites(&self) -> Vec<FrameSite> {
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
            sites.push(FrameSite::Instruction {
                function_id: native.header.function_id,
                pc,
            });
            if let Some(record) = call {
                sites.extend(record.inline_frames.iter().map(|frame| FrameSite::Inline {
                    function_id: frame.function_id,
                    byte_pc: frame.byte_pc,
                }));
            }
        }
        sites.reverse();
        sites
    }

    pub(crate) fn visit_active_frame_snapshots(
        &self,
        context: &ExecutionContext,
        limit: usize,
        visit: impl FnMut(StackFrameSnapshotView<'_>) -> bool,
    ) {
        visit_frame_sites(context, &self.capture_active_sites(), limit, visit);
    }
}

/// Owned snapshots of `sites` (innermost first), at most `limit` frames.
pub(crate) fn resolve_frame_sites(
    context: &ExecutionContext,
    sites: &[FrameSite],
    limit: usize,
) -> Vec<StackFrameSnapshot> {
    let mut result = Vec::new();
    visit_frame_sites(context, sites, limit, |frame| {
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

fn visit_frame_sites(
    context: &ExecutionContext,
    sites: &[FrameSite],
    limit: usize,
    mut visit: impl FnMut(StackFrameSnapshotView<'_>) -> bool,
) {
    let resolved = sites.iter().filter_map(|site| site.instruction(context));
    for (function, pc) in resolved.take(limit) {
        if !crate::stack_snapshot::visit_frame_snapshot(context, function, pc as usize, &mut visit)
        {
            break;
        }
    }
}
