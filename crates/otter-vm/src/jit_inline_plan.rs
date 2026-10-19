//! Which inline candidates one optimizing compilation builds in place of
//! their calls.
//!
//! The compiled function's call sites are candidates, and an admitted
//! candidate's own call sites are candidates below it. Every candidate has a
//! frequency, how often its call runs per run of the compiled function (the
//! product of the per-entry call frequencies along its path), and a size in
//! bytecode instructions: its own, plus what its function's installed
//! optimized code inlined, which building the body in place builds again.
//!
//! The same choice runs twice over the same feedback: on the isolate
//! thread, before snapshots are prepared, so only chosen bodies are
//! prepared; and in the compiler over the prepared tree.
//!
//! # Contents
//! - [`InlineCandidate`] / [`InlineTree`] — one candidate and the tree that
//!   lists a body's candidates.
//! - [`choose_inlined`] — the admitted candidates.
//!
//! # Invariants
//! - A candidate below [`crate::jit::JIT_MIN_INLINING_FREQUENCY`] is never
//!   admitted unless it is small, nor is anything below it. A small body
//!   costs no more built in place than its call does, and the frequencies
//!   come from runs before the function was optimized, which may not have
//!   reached a branch that later turns hot.
//! - A small body (at most [`crate::jit::JIT_SMALL_INLINE_INSTRUCTIONS`]) is
//!   admitted as soon as it is reached and charges only the absolute budget,
//!   [`crate::jit::JIT_MAX_INLINED_INSTRUCTIONS_ABSOLUTE`].
//! - Other bodies are admitted in decreasing frequency per instruction of
//!   their size while the cumulative budget,
//!   [`crate::jit::JIT_MAX_INLINED_INSTRUCTIONS_CUMULATIVE`], holds their
//!   size with a fifth more in reserve for the small bodies they expose. An
//!   admitted body charges its own instructions; the bodies below it charge
//!   theirs when admitted. A body that does not fit is skipped and smaller
//!   ones are still tried.
//! - Only an admitted body's own candidates become candidates.
//! - A site without call feedback has an unknown frequency; unknown
//!   candidates rank below every known one, smaller first.
//! - A recursive candidate, whose function already runs on its inline path,
//!   is sized by its own body: what its optimized code inlined is this very
//!   expansion. One that already runs [`crate::jit::JIT_INLINE_RECURSION`]
//!   times there is never admitted: the graph builder would not build it.
//!
//! # See also
//! - `otter_jit::graph::inline_plan` lists prepared bodies as the tree.
//! - `crate::interp::inline_snapshot_budget` lists feedback as the tree.

use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::hash::Hash;

use rustc_hash::FxHashSet;

use crate::jit::{
    JIT_INLINE_RECURSION, JIT_MAX_INLINED_INSTRUCTIONS_ABSOLUTE,
    JIT_MAX_INLINED_INSTRUCTIONS_CUMULATIVE, JIT_MIN_INLINING_FREQUENCY,
    JIT_SMALL_INLINE_INSTRUCTIONS,
};

/// One body that may be built in place of its call.
#[derive(Debug, Clone)]
pub struct InlineCandidate<K> {
    /// The tree's identity of the candidate.
    pub key: K,
    /// Calls per entry of the body holding the site, when it has feedback.
    pub site_frequency: Option<f32>,
    /// Bytecode instructions of the candidate's own body.
    pub instructions: u32,
    /// Bytecode instructions the function's installed optimized code inlined.
    pub optimized_inlined_instructions: u32,
    /// How many bodies on the candidate's inline path, the compiled function
    /// included, run its function.
    pub recursion: usize,
}

/// The candidates of every body.
pub trait InlineTree {
    /// Identity of one candidate; the compiled function has none.
    type Key: Clone + Eq + Hash;

    /// The candidates of the body `body` names, `None` for the compiled
    /// function, in a deterministic order.
    fn candidates(&mut self, body: Option<&Self::Key>) -> Vec<InlineCandidate<Self::Key>>;
}

struct Queued<K> {
    candidate: InlineCandidate<K>,
    frequency: Option<f32>,
    size: u32,
    order: u32,
}

impl<K> Queued<K> {
    fn score(&self) -> Option<f32> {
        self.frequency
            .map(|frequency| frequency / self.size.max(1) as f32)
    }
}

impl<K> PartialEq for Queued<K> {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl<K> Eq for Queued<K> {}

impl<K> PartialOrd for Queued<K> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl<K> Ord for Queued<K> {
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

struct Chooser<'t, T: InlineTree> {
    tree: &'t mut T,
    queue: BinaryHeap<Queued<T::Key>>,
    admitted: FxHashSet<T::Key>,
    /// Instructions of every admitted body.
    total: u32,
    /// Instructions of the admitted bodies that are not small.
    cumulative: u32,
    order: u32,
}

impl<T: InlineTree> Chooser<'_, T> {
    /// Offer the candidates of `body`, which runs `frequency` times per run
    /// of the compiled function.
    fn offer_candidates(&mut self, body: Option<&T::Key>, frequency: Option<f32>) {
        for candidate in self.tree.candidates(body) {
            let frequency = match (frequency, candidate.site_frequency) {
                (Some(caller), Some(site)) => Some(caller * site),
                (caller, site) => caller.or(site),
            };
            if candidate.recursion >= JIT_INLINE_RECURSION {
                continue;
            }
            let small = candidate.instructions <= JIT_SMALL_INLINE_INSTRUCTIONS;
            if !small && frequency.is_some_and(|frequency| frequency < JIT_MIN_INLINING_FREQUENCY) {
                continue;
            }
            if small {
                if self.total + candidate.instructions <= JIT_MAX_INLINED_INSTRUCTIONS_ABSOLUTE {
                    self.admit(candidate, frequency);
                }
                continue;
            }
            let size = if candidate.recursion > 0 {
                candidate.instructions
            } else {
                candidate
                    .instructions
                    .saturating_add(candidate.optimized_inlined_instructions)
            };
            self.order += 1;
            self.queue.push(Queued {
                candidate,
                frequency,
                size,
                order: self.order,
            });
        }
    }

    fn admit(&mut self, candidate: InlineCandidate<T::Key>, frequency: Option<f32>) {
        self.total += candidate.instructions;
        let key = candidate.key;
        self.admitted.insert(key.clone());
        self.offer_candidates(Some(&key), frequency);
    }

    fn run(mut self) -> FxHashSet<T::Key> {
        self.offer_candidates(None, Some(1.0));
        while let Some(queued) = self.queue.pop() {
            let reserved = queued.size.saturating_add(queued.size / 5);
            if self.cumulative.saturating_add(reserved) > JIT_MAX_INLINED_INSTRUCTIONS_CUMULATIVE
                || self.total + queued.candidate.instructions
                    > JIT_MAX_INLINED_INSTRUCTIONS_ABSOLUTE
            {
                continue;
            }
            self.cumulative += queued.candidate.instructions;
            self.admit(queued.candidate, queued.frequency);
        }
        self.admitted
    }
}

/// The candidates of `tree` one compilation inlines.
#[must_use]
pub fn choose_inlined<T: InlineTree>(tree: &mut T) -> FxHashSet<T::Key> {
    Chooser {
        tree,
        queue: BinaryHeap::new(),
        admitted: FxHashSet::default(),
        total: 0,
        cumulative: 0,
        order: 0,
    }
    .run()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tree of named bodies: each entry lists a body's candidates.
    struct Fixture(Vec<(Option<&'static str>, Vec<InlineCandidate<&'static str>>)>);

    impl InlineTree for Fixture {
        type Key = &'static str;

        fn candidates(
            &mut self,
            body: Option<&&'static str>,
        ) -> Vec<InlineCandidate<&'static str>> {
            self.0
                .iter()
                .find(|(owner, _)| owner.as_ref() == body)
                .map(|(_, candidates)| candidates.clone())
                .unwrap_or_default()
        }
    }

    fn candidate(
        key: &'static str,
        frequency: f32,
        instructions: u32,
    ) -> InlineCandidate<&'static str> {
        InlineCandidate {
            key,
            site_frequency: Some(frequency),
            instructions,
            optimized_inlined_instructions: 0,
            recursion: 0,
        }
    }

    #[test]
    fn hot_bodies_take_the_budget_before_cold_ones() {
        let mut tree = Fixture(vec![
            (
                None,
                vec![
                    candidate("cold", 1.0, 200),
                    candidate("loop", 1.0, 100),
                    candidate("rare", 0.05, 40),
                    candidate("tiny", 0.01, 4),
                ],
            ),
            (Some("loop"), vec![candidate("hot", 100.0, 150)]),
        ]);
        let chosen = choose_inlined(&mut tree);
        // `loop` ranks above `cold` (1/100 > 1/200) and exposes `hot`, which
        // then fills the budget before `cold`; `rare` is too rare; a small
        // body is always taken.
        assert!(chosen.contains("loop") && chosen.contains("hot") && chosen.contains("tiny"));
        assert!(!chosen.contains("cold") && !chosen.contains("rare"));
    }
}
