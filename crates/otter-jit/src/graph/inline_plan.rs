//! Which prepared inline bodies one optimizing compilation builds in place
//! of their calls.
//!
//! The snapshot of the compiled function holds a tree of prepared bodies:
//! each call or method site with a baked body is a candidate, and each
//! candidate's own sites are candidates below it. The choice is
//! [`otter_vm::jit_inline_plan::choose_inlined`] over that tree, the same
//! choice that decided which bodies to prepare.
//!
//! # Contents
//! - [`InlinePlan`] — the admitted bodies, chosen before the graph is built.
//!
//! # Invariants
//! - A candidate's site frequency is its caller snapshot's baked frequency
//!   at the site's byte PC; its size counts what its function's optimized
//!   code inlined, unless its function already runs on its inline path.
//! - Sites are offered in byte PC order, then by function: map order never
//!   decides a tie.
//!
//! # See also
//! - [`super::builder`] builds an admitted body where its call stands.
//! - `otter_vm::interp::inline_snapshot_budget` prepares the candidates.

use std::sync::Arc;

use otter_vm::jit::JitCompileSnapshot;
use otter_vm::jit_inline_plan::{InlineCandidate, InlineTree, choose_inlined};
use rustc_hash::{FxHashMap, FxHashSet};
use smallvec::SmallVec;

/// The prepared bodies one compilation inlines, by snapshot identity.
#[derive(Default)]
pub(crate) struct InlinePlan {
    admitted: FxHashSet<*const JitCompileSnapshot>,
}

/// The functions of the bodies an inline path runs through.
type Path = SmallVec<[u32; 8]>;

/// The prepared bodies as a candidate tree.
struct SnapshotTree<'a> {
    root: &'a JitCompileSnapshot,
    /// Each offered body and the functions on its path, itself excluded.
    bodies: FxHashMap<*const JitCompileSnapshot, (&'a JitCompileSnapshot, Path)>,
}

impl InlineTree for SnapshotTree<'_> {
    type Key = *const JitCompileSnapshot;

    fn candidates(&mut self, body: Option<&Self::Key>) -> Vec<InlineCandidate<Self::Key>> {
        let (view, mut path) = match body {
            None => (self.root, Path::new()),
            Some(key) => self.bodies[key].clone(),
        };
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
        let mut sites: Vec<(u32, &Arc<JitCompileSnapshot>)> = plain.chain(methods).collect();
        sites.sort_by_key(|&(byte_pc, body)| (byte_pc, body.code_block.id));
        sites
            .into_iter()
            .map(|(byte_pc, body)| {
                let key = Arc::as_ptr(body);
                self.bodies.insert(key, (body.as_ref(), path.clone()));
                InlineCandidate {
                    key,
                    site_frequency: view.call_frequencies.get(&byte_pc).copied(),
                    instructions: body.code_block.instruction_count(),
                    optimized_inlined_instructions: body.optimized_inlined_instructions,
                    recursion: path
                        .iter()
                        .filter(|&&function_id| function_id == body.code_block.id)
                        .count(),
                }
            })
            .collect()
    }
}

impl InlinePlan {
    /// Choose the bodies the compilation of `root` inlines.
    #[must_use]
    pub(crate) fn choose(root: &JitCompileSnapshot) -> Self {
        let mut tree = SnapshotTree {
            root,
            bodies: FxHashMap::default(),
        };
        Self {
            admitted: choose_inlined(&mut tree),
        }
    }

    /// Whether `body` was chosen.
    #[must_use]
    pub(crate) fn admits(&self, body: &JitCompileSnapshot) -> bool {
        self.admitted.contains(&std::ptr::from_ref(body))
    }
}
