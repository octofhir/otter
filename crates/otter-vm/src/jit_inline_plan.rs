//! Which inline candidates one optimizing compilation builds in place of
//! their calls.
//!
//! The compiled function's call sites are candidates, and an admitted
//! candidate's own call sites are candidates below it. Every candidate has a
//! frequency, how often its call runs per run of the compiled function (the
//! product of the per-entry call frequencies along its path), and a size in
//! bytecode instructions: its own body's. The bodies it calls are candidates
//! of their own, each sized and charged when it is admitted, so what a
//! callee's own optimized code happened to inline never decides whether the
//! callee is built in place: that would make the choice depend on which
//! function finished compiling first.
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
//! - A body that is not small, runs at most once per run of the compiled
//!   function and itself makes more than
//!   [`crate::jit::JIT_MAX_INLINED_CALLEE_INVOKES`] calls per entry is never
//!   admitted: building it in place saves one call, and it is compiled on
//!   its own, where its calls have a budget.
//! - Only an admitted body's own candidates become candidates.
//! - A site without call feedback has an unknown frequency; unknown
//!   candidates rank below every known one, smaller first.
//! - A candidate that already runs [`crate::jit::JIT_INLINE_RECURSION`] times
//!   on its inline path is never admitted: the graph builder would not build
//!   it.
//!
//! # See also
//! - `otter_jit::graph::inline_plan` lists prepared bodies as the tree.
//! - `crate::interp::inline_snapshot_budget` lists feedback as the tree.

use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::hash::Hash;

use rustc_hash::FxHashSet;

use crate::jit::{
    JIT_INLINE_RECURSION, JIT_MAX_INLINED_CALLEE_INVOKES, JIT_MAX_INLINED_INSTRUCTIONS_ABSOLUTE,
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
    /// Calls the candidate's own body makes per entry, summed over its call
    /// sites with feedback.
    pub invokes: f32,
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
            // Building a body entered at most once per run in place saves one
            // call; its own calls would then compete for this compilation's
            // budget instead of its own.
            if !small
                && candidate.invokes > JIT_MAX_INLINED_CALLEE_INVOKES
                && frequency.is_none_or(|frequency| frequency <= 1.0)
            {
                continue;
            }
            if small {
                if self.total + candidate.instructions <= JIT_MAX_INLINED_INSTRUCTIONS_ABSOLUTE {
                    self.admit(candidate, frequency);
                }
                continue;
            }
            let size = candidate.instructions;
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
            invokes: 0.0,
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

    #[test]
    fn a_once_entered_body_that_makes_many_calls_is_compiled_on_its_own() {
        let calling = |key, frequency, invokes| InlineCandidate {
            invokes,
            ..candidate(key, frequency, 60)
        };
        let mut tree = Fixture(vec![(
            None,
            vec![
                calling("driver", 1.0, 400.0),
                calling("hot", 300.0, 6.0),
                calling("plain", 1.0, 2.0),
            ],
        )]);
        let chosen = choose_inlined(&mut tree);
        // The driver saves one call; the hot body saves hundreds.
        assert!(!chosen.contains("driver"));
        assert!(chosen.contains("hot") && chosen.contains("plain"));
    }
}
