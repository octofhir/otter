//! Shared resource bound for owned plain and method inline snapshot trees,
//! and the choice of the bodies an optimizing compilation prepares.
//!
//! # Contents
//! - [`InlineSnapshotBudget`] — maximum nesting, deeper for small bodies, a
//!   per-root bound on prepared bodies, the call frequency of the body being
//!   baked, and the chosen inline paths when a choice was made.
//! - [`FeedbackTree`] — the call and method sites of bodies, read from their
//!   feedback, as the candidate tree [`crate::jit_inline_plan`] chooses from.
//!
//! # Invariants
//! - Every candidate consumes the same budget before preparing its descendants.
//! - Rejected or failed candidates never replenish work already performed.
//! - Each body is a fresh owned snapshot; depth bounds recursive preparation.
//! - A candidate's frequency is its site's calls per entry of the body that
//!   holds it, times that body's own frequency; the root runs once. A site
//!   below [`crate::jit::JIT_MIN_INLINING_FREQUENCY`] is never prepared
//!   unless its body is small: no optimizing caller inlines it. A site
//!   without call feedback is unknown and passes.
//! - With a choice, a body that is not small is prepared only on a chosen
//!   path: the compiler chooses again over the prepared tree with the same
//!   feedback, so preparing more would only cost the isolate thread.
//! - Graph lowering independently chooses, by frequency, which prepared
//!   bodies it inlines.
//!
//! # See also
//! - `jit_compile::bake_inline_body` owns each descendant's source and feedback.

use crate::jit_inline_plan::{InlineCandidate, InlineTree};
use crate::{ExecutionContext, Interpreter};
use otter_bytecode::Op;

/// The call sites from the compiled function to one inline body: each
/// site's byte PC and the function it calls.
pub(super) type InlinePath = smallvec::SmallVec<[(u32, u32); 6]>;

pub(super) struct InlineSnapshotBudget {
    remaining: usize,
    /// Frequency of every entered body, outermost first, relative to one
    /// run of the root.
    frequencies: smallvec::SmallVec<[f32; 8]>,
    /// The chosen inline paths, when a choice was made.
    chosen: Option<rustc_hash::FxHashSet<InlinePath>>,
    /// The path of the body being baked.
    path: InlinePath,
    /// Depth of the outermost entered body prepared off the chosen paths,
    /// whose subtree the choice does not cover.
    unchosen_from: Option<usize>,
}

impl InlineSnapshotBudget {
    pub(super) fn new() -> Self {
        Self {
            remaining: 64,
            frequencies: smallvec::SmallVec::new(),
            chosen: None,
            path: InlinePath::new(),
            unchosen_from: None,
        }
    }

    /// A budget that prepares, besides small bodies, only `chosen` paths.
    pub(super) fn chosen(chosen: rustc_hash::FxHashSet<InlinePath>) -> Self {
        Self {
            remaining: 256,
            chosen: Some(chosen),
            ..Self::new()
        }
    }

    /// Admit the body of `function_id`, of `instructions` bytecode
    /// instructions, called at `byte_pc` of the current body, which ran that
    /// call `site_frequency` times per entry. A body without a site (a
    /// receiver's inherited method) is outside the choice, with its subtree.
    pub(super) fn enter(
        &mut self,
        byte_pc: Option<u32>,
        function_id: u32,
        instructions: u32,
        site_frequency: Option<f32>,
    ) -> bool {
        let small = instructions <= crate::jit::JIT_SMALL_INLINE_INSTRUCTIONS;
        let depth = if small {
            crate::jit::JIT_SMALL_INLINE_DEPTH
        } else {
            crate::jit::JIT_INLINE_DEPTH
        };
        let caller = self.frequencies.last().copied().unwrap_or(1.0);
        let frequency = site_frequency.map_or(caller, |site| caller * site);
        if self.frequencies.len() >= usize::from(depth)
            || self.remaining == 0
            || (!small && frequency < crate::jit::JIT_MIN_INLINING_FREQUENCY)
        {
            return false;
        }
        self.path.push((byte_pc.unwrap_or(u32::MAX), function_id));
        if byte_pc.is_none() && self.unchosen_from.is_none() {
            self.unchosen_from = Some(self.path.len());
        }
        if !small
            && self.unchosen_from.is_none()
            && self
                .chosen
                .as_ref()
                .is_some_and(|chosen| !chosen.contains(&self.path))
        {
            self.path.pop();
            return false;
        }
        self.remaining -= 1;
        self.frequencies.push(frequency);
        true
    }

    pub(super) fn leave(&mut self) {
        self.frequencies
            .pop()
            .expect("inline preparation leaves an entered body");
        if self.unchosen_from == Some(self.path.len()) {
            self.unchosen_from = None;
        }
        self.path.pop();
    }
}

/// The call and method sites of bodies, read from their feedback.
pub(super) struct FeedbackTree<'a> {
    pub(super) interp: &'a Interpreter,
    pub(super) context: &'a ExecutionContext,
    pub(super) root: u32,
}

/// How a candidate's function must admit being built in place.
#[derive(Clone, Copy)]
enum SiteKind {
    Plain,
    Construct,
    Method,
}

impl FeedbackTree<'_> {
    /// Run `f` over the body of `function_id`.
    fn with_body<R>(
        &self,
        function_id: u32,
        f: impl FnOnce(&crate::CodeBlock) -> Option<R>,
    ) -> Option<R> {
        let owner = self.context.for_function(function_id).ok()?;
        f(owner.exec_function(function_id)?)
    }

    /// The sites of `code_block` that may hold an inline candidate.
    fn sites(&self, code_block: &crate::CodeBlock) -> Vec<(u32, u32, Option<f32>, SiteKind)> {
        let mut sites = Vec::new();
        for (index, instruction) in code_block.code.iter().enumerate() {
            let op = code_block.op(instruction);
            let Some(byte_pc) = code_block.instruction_byte_pc(index) else {
                continue;
            };
            let site_frequency = code_block.call_frequency_at(index);
            if op == Op::CallMethodValue {
                match instruction
                    .property_ic_site()
                    .and_then(|site| self.interp.method_target_feedback(site))
                {
                    Some(crate::MethodCallFeedback::Mono { method_fid, .. }) => {
                        sites.push((byte_pc, method_fid, site_frequency, SiteKind::Method));
                    }
                    Some(crate::MethodCallFeedback::Poly(targets)) => {
                        sites.extend(targets.iter().map(|target| {
                            (byte_pc, target.method_fid, site_frequency, SiteKind::Method)
                        }))
                    }
                    _ => {}
                }
            }
            let kind = match op {
                Op::New => SiteKind::Construct,
                Op::NewSpread | Op::SuperConstruct | Op::SuperConstructSpread | Op::CallSpread => {
                    continue;
                }
                _ => SiteKind::Plain,
            };
            let Some(crate::feedback::CallSiteDistribution::Mono(target)) =
                code_block.call_distribution_at(index)
            else {
                continue;
            };
            if let crate::feedback::OrdinaryCallTarget::Bytecode(callee)
            | crate::feedback::OrdinaryCallTarget::FunctionPrototypeCall(callee) = target.target
            {
                sites.push((byte_pc, callee, site_frequency, kind));
            }
        }
        sites
    }
}

impl InlineTree for FeedbackTree<'_> {
    type Key = InlinePath;

    fn candidates(&mut self, body: Option<&InlinePath>) -> Vec<InlineCandidate<InlinePath>> {
        let path = body.cloned().unwrap_or_default();
        let function_id = path
            .last()
            .map_or(self.root, |&(_, function_id)| function_id);
        let sites = self
            .with_body(function_id, |code_block| Some(self.sites(code_block)))
            .unwrap_or_default();
        sites
            .into_iter()
            .filter_map(|(byte_pc, callee, site_frequency, kind)| {
                let instructions = self.with_body(callee, |code_block| {
                    let admitted = code_block.admits_graph_inlining()
                        && match kind {
                            SiteKind::Plain => {
                                code_block
                                    .admits_generated_call(crate::jit::JitDirectCallKind::Plain)
                                    && code_block.splices_without_argument_frame()
                            }
                            SiteKind::Construct => {
                                code_block
                                    .admits_generated_call(crate::jit::JitDirectCallKind::Construct)
                                    && code_block.splices_without_argument_frame()
                                    && !code_block.is_derived_constructor
                                    && !code_block.is_arrow()
                            }
                            SiteKind::Method => {
                                code_block
                                    .admits_generated_call(crate::jit::JitDirectCallKind::Method)
                                    && !code_block.requires_argument_frame()
                                    // A direct method links an existing entry.
                                    && self.interp.current_direct_callee_plan(code_block).is_some()
                            }
                        };
                    admitted.then(|| code_block.instruction_count())
                })?;
                let mut key = path.clone();
                key.push((byte_pc, callee));
                Some(InlineCandidate {
                    key,
                    site_frequency,
                    instructions,
                    optimized_inlined_instructions: self
                        .interp
                        .jit_optimized_code
                        .get(&callee)
                        .and_then(Option::as_ref)
                        .map_or(0, |code| code.inlined_instructions()),
                    recursion: usize::from(callee == self.root)
                        + path.iter().filter(|&&(_, caller)| caller == callee).count(),
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_work_is_bounded_across_siblings_and_nesting() {
        let mut budget = InlineSnapshotBudget::new();
        for _ in 0..crate::jit::JIT_INLINE_DEPTH {
            assert!(budget.enter(Some(0), 0, 20, None));
        }
        assert!(!budget.enter(Some(0), 0, 20, None));
        // Small bodies nest deeper.
        for _ in crate::jit::JIT_INLINE_DEPTH..crate::jit::JIT_SMALL_INLINE_DEPTH {
            assert!(budget.enter(Some(0), 0, 4, None));
        }
        assert!(!budget.enter(Some(0), 0, 4, None));
        for _ in 0..crate::jit::JIT_SMALL_INLINE_DEPTH {
            budget.leave();
        }
        for _ in crate::jit::JIT_SMALL_INLINE_DEPTH..64 {
            assert!(budget.enter(Some(0), 0, 20, None));
            budget.leave();
        }
        assert!(!budget.enter(Some(0), 0, 4, None));
    }

    #[test]
    fn a_choice_prepares_only_chosen_paths_and_small_bodies() {
        let chosen = [
            InlinePath::from_slice(&[(8, 1)]),
            InlinePath::from_slice(&[(8, 1), (4, 2)]),
        ];
        let mut budget = InlineSnapshotBudget::chosen(chosen.into_iter().collect());
        assert!(!budget.enter(Some(8), 2, 20, None));
        assert!(budget.enter(Some(8), 1, 20, None));
        assert!(budget.enter(Some(4), 2, 20, None));
        budget.leave();
        assert!(!budget.enter(Some(6), 2, 20, None));
        assert!(budget.enter(Some(6), 3, 4, None));
        budget.leave();
        // A receiver's inherited method and its subtree are outside the choice.
        assert!(budget.enter(None, 5, 20, None));
        assert!(budget.enter(Some(2), 6, 20, None));
        budget.leave();
        budget.leave();
        assert!(!budget.enter(Some(2), 6, 20, None));
    }

    #[test]
    fn rarely_reached_sites_are_not_prepared() {
        let mut budget = InlineSnapshotBudget::new();
        assert!(!budget.enter(Some(0), 0, 20, Some(0.05)));
        // A small body costs no more inlined than its call.
        assert!(budget.enter(Some(0), 0, 4, Some(0.05)));
        budget.leave();
        assert!(budget.enter(Some(0), 0, 20, Some(0.5)));
        // A site run once per entry of a body reached half the time.
        assert!(budget.enter(Some(0), 0, 20, Some(1.0)));
        budget.leave();
        assert!(!budget.enter(Some(0), 0, 20, Some(0.15)));
        assert!(budget.enter(Some(0), 0, 20, Some(4.0)));
    }
}
