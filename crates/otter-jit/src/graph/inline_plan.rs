//! Which prepared inline bodies one optimizing compilation builds in place
//! of their calls.
//!
//! The snapshot of the compiled function holds a tree of prepared bodies:
//! each call or method site with a baked body is a candidate, and each
//! candidate's own sites are candidates below it. Every candidate has a
//! frequency, how often its call runs per run of the compiled function (the
//! product of the per-entry call frequencies along its path), and a size in
//! bytecode instructions: its own, plus what the function's installed
//! optimized code inlined, which building the body here builds again.
//!
//! # Contents
//! - [`InlinePlan`] — the admitted bodies, chosen before the graph is built.
//!
//! # Invariants
//! - A candidate below [`otter_vm::jit::JIT_MIN_INLINING_FREQUENCY`] is never
//!   admitted unless it is small, nor is anything below it. A small body
//!   costs no more built in place than its call does, and the frequencies
//!   come from runs before the function was optimized, which may not have
//!   reached a branch that later turns hot.
//! - A small body (at most [`otter_vm::jit::JIT_SMALL_INLINE_INSTRUCTIONS`])
//!   is admitted as soon as it is reached and charges only the absolute
//!   budget, [`otter_vm::jit::JIT_MAX_INLINED_INSTRUCTIONS_ABSOLUTE`].
//! - Other bodies are admitted in decreasing frequency per instruction of
//!   their size while the cumulative budget,
//!   [`otter_vm::jit::JIT_MAX_INLINED_INSTRUCTIONS_CUMULATIVE`], holds their
//!   size with a fifth more in reserve for the small bodies they expose. An
//!   admitted body charges its own instructions; the bodies below it charge
//!   theirs when admitted. A body that does not fit is skipped and smaller
//!   ones are still tried.
//! - Only an admitted body's own candidates become candidates.
//! - A site without call feedback has an unknown frequency; unknown
//!   candidates rank below every known one, smaller first.
//! - A recursive candidate, whose function already runs on its inline path,
//!   is sized by its own body: what its optimized code inlined is this very
//!   expansion.
//!
//! # See also
//! - [`super::builder`] builds an admitted body where its call stands.
//! - `otter_vm::interp::inline_snapshot_budget` prepares the candidates.

use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::sync::Arc;

use otter_vm::jit::{
    JIT_MAX_INLINED_INSTRUCTIONS_ABSOLUTE, JIT_MAX_INLINED_INSTRUCTIONS_CUMULATIVE,
    JIT_MIN_INLINING_FREQUENCY, JIT_SMALL_INLINE_INSTRUCTIONS, JitCompileSnapshot,
};
use rustc_hash::FxHashSet;
use smallvec::SmallVec;

/// The prepared bodies one compilation inlines, by snapshot identity.
#[derive(Default)]
pub(crate) struct InlinePlan {
    admitted: FxHashSet<*const JitCompileSnapshot>,
}

/// A candidate body awaiting its turn.
struct Candidate<'a> {
    body: &'a Arc<JitCompileSnapshot>,
    /// Calls per run of the compiled function, when every site on the path
    /// has feedback.
    frequency: Option<f32>,
    instructions: u32,
    /// Its own instructions and those its function's optimized code inlined.
    size: u32,
    /// Discovery order: the earlier candidate wins a tie.
    order: u32,
    /// Functions of the bodies on its inline path, the compiled one first.
    path: Path,
}

/// The functions of the bodies an inline path runs through.
type Path = SmallVec<[u32; 8]>;

impl Candidate<'_> {
    fn score(&self) -> Option<f32> {
        self.frequency
            .map(|frequency| frequency / self.size.max(1) as f32)
    }
}

impl PartialEq for Candidate<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Candidate<'_> {}

impl PartialOrd for Candidate<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Candidate<'_> {
    /// Greater is admitted first.
    fn cmp(&self, other: &Self) -> Ordering {
        match (self.score(), other.score()) {
            (Some(a), Some(b)) => a.total_cmp(&b),
            (Some(_), None) => Ordering::Greater,
            (None, Some(_)) => Ordering::Less,
            (None, None) => other.size.cmp(&self.size),
        }
        .then_with(|| other.order.cmp(&self.order))
    }
}

struct Planner<'a> {
    queue: BinaryHeap<Candidate<'a>>,
    plan: InlinePlan,
    /// Instructions of every admitted body.
    total: u32,
    /// Instructions of the admitted bodies that are not small.
    cumulative: u32,
    order: u32,
}

impl<'a> Planner<'a> {
    /// Offer the candidates of `view`, a body that runs `frequency` times
    /// per run of the compiled function.
    fn offer_sites(&mut self, view: &'a JitCompileSnapshot, frequency: Option<f32>, path: &Path) {
        let mut path = path.clone();
        path.push(view.code_block.id);
        let plain = view
            .inline_callees
            .iter()
            .map(|(&byte_pc, callee)| (byte_pc, &callee.body));
        let methods = view.direct_methods.iter().flat_map(|(&byte_pc, methods)| {
            methods
                .iter()
                .filter_map(move |method| Some((byte_pc, method.body.as_ref()?)))
        });
        let mut sites: Vec<(u32, &'a Arc<JitCompileSnapshot>)> = plain.chain(methods).collect();
        // Map order is not program order; discovery order breaks ties.
        sites.sort_by_key(|&(byte_pc, body)| (byte_pc, body.code_block.id));
        for (byte_pc, body) in sites {
            let site = view.call_frequencies.get(&byte_pc).copied();
            let frequency = match (frequency, site) {
                (Some(caller), Some(site)) => Some(caller * site),
                (None, _) | (_, None) => frequency.or(site),
            };
            self.offer(body, frequency, &path);
        }
    }

    fn offer(&mut self, body: &'a Arc<JitCompileSnapshot>, frequency: Option<f32>, path: &Path) {
        let instructions = body.code_block.instruction_count();
        let small = instructions <= JIT_SMALL_INLINE_INSTRUCTIONS;
        if !small && frequency.is_some_and(|frequency| frequency < JIT_MIN_INLINING_FREQUENCY) {
            return;
        }
        if small {
            if self.total + instructions <= JIT_MAX_INLINED_INSTRUCTIONS_ABSOLUTE {
                self.admit(body, instructions, frequency, path);
            }
            return;
        }
        let recursive = path.contains(&body.code_block.id);
        self.order += 1;
        self.queue.push(Candidate {
            body,
            frequency,
            instructions,
            size: if recursive {
                instructions
            } else {
                instructions.saturating_add(body.optimized_inlined_instructions)
            },
            order: self.order,
            path: path.clone(),
        });
    }

    fn admit(
        &mut self,
        body: &'a Arc<JitCompileSnapshot>,
        instructions: u32,
        frequency: Option<f32>,
        path: &Path,
    ) {
        self.plan.admitted.insert(Arc::as_ptr(body));
        self.total += instructions;
        self.offer_sites(body, frequency, path);
    }

    fn run(mut self) -> InlinePlan {
        while let Some(candidate) = self.queue.pop() {
            let reserved = candidate.size.saturating_add(candidate.size / 5);
            if self.cumulative.saturating_add(reserved) > JIT_MAX_INLINED_INSTRUCTIONS_CUMULATIVE
                || self.total + candidate.instructions > JIT_MAX_INLINED_INSTRUCTIONS_ABSOLUTE
            {
                continue;
            }
            self.cumulative += candidate.instructions;
            self.admit(
                candidate.body,
                candidate.instructions,
                candidate.frequency,
                &candidate.path,
            );
        }
        self.plan
    }
}

impl InlinePlan {
    /// Choose the bodies the compilation of `root` inlines.
    #[must_use]
    pub(crate) fn choose(root: &JitCompileSnapshot) -> Self {
        let mut planner = Planner {
            queue: BinaryHeap::new(),
            plan: Self::default(),
            total: 0,
            cumulative: 0,
            order: 0,
        };
        planner.offer_sites(root, Some(1.0), &Path::new());
        planner.run()
    }

    /// Whether `body` was chosen.
    #[must_use]
    pub(crate) fn admits(&self, body: &JitCompileSnapshot) -> bool {
        self.admitted.contains(&std::ptr::from_ref(body))
    }
}
