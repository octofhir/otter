//! Graph construction: an abstract interpretation of the bytecode that keeps
//! one SSA value per interpreter register and turns feedback into
//! speculation.
//!
//! # Contents
//! - [`build`] — the entry point: bytecode analysis plus snapshot feedback to
//!   a finished [`Graph`] whose blocks are in emission order.
//! - The per-opcode lowering in [`Builder::visit`]: every instruction becomes
//!   specialized nodes, a [`Kind::Generic`] node, or an unconditional deopt.
//! - [`Known`] — facts about values that hold along the current path
//!   (alternative representations, proved shapes and Object receivers), merged
//!   at joins.
//!
//! # Invariants
//! - No instruction makes the whole function fail: anything without a
//!   specialized form becomes a `Generic` node, and an operation that cannot
//!   run in optimized code at all (suspension, an irreducible entry) becomes
//!   an unconditional deopt that resumes the interpreter at its PC.
//! - Blocks are visited in reverse post-order, so every forward predecessor
//!   of a join has been visited before the join starts; a join's phis are
//!   created from the complete set of forward states, and back edges append
//!   their inputs when they arrive.
//! - Frame states carry exactly the registers live before (eager) or after
//!   (lazy) the instruction; dead registers resume as `undefined`.
//! - A fact in [`Known`] never outlives the path it was proved on: joins keep
//!   only facts every predecessor agrees on, calls drop heap facts except
//!   shape memberships of stable shapes, which the generation subscribes to, and
//!   a store forgets every known value at its offset. A loop header keeps the
//!   heap facts of its entry only when every back edge arrives with them;
//!   otherwise the graph is rebuilt with the facts a back edge lost forgotten
//!   at that header, or all of them for a loop that calls out. Storage words
//!   read from the heap never cross a back edge, whose interrupt poll may
//!   move element storage; context parents never change and are always kept.
//! - The finished graph has no trivial phi: one that merges a single value
//!   (apart from itself) is replaced by that value everywhere.
//! - A receiver proved to be an ECMAScript Object needs no sloppy `this`
//!   conversion. The proof belongs to its SSA value, survives heap mutation,
//!   and is intersected at joins. An inlined LoadThis inherits the actual
//!   caller binding instead of assuming its callee has already converted it.
//! - Physical object/array body kinds are immutable facts of SSA values,
//!   proved by their allocation or a successful kind check. They survive
//!   calls and moving collection; layouts and array lengths remain mutable.
//! - Empty property programs do not prove a cold branch. Only a site whose
//!   source-owned attempt state is unset can leave for insufficient feedback.
//! - A proven global read keeps its actual source snapshot and byte PC.
//!   Its live value is effectful; a pre-effect guard resumes that source
//!   binding operation, including the full chain of inlined frames.
//! - Own fields carry their immutable bank and index in one access node; no
//!   movable suffix address escapes into SSA or crosses a collecting boundary.
//! - Intrinsic call operations follow a traced proof of the evaluated callee
//!   in the current source realm; guards reconstruct the original call and
//!   evaluated actuals before any specialized effect.
//! - Named loads select feedback programs by current or revalidated receiver
//!   shapes. Their indices belong to the innermost source snapshot; only an
//!   exact singleton proof removes the selected program's receiver guard.
//! - Shape-owned field bounds belong to loaded SSA values. Their nullish
//!   arms require edge refinement or an eager check, and their ordinary
//!   targets require live stability checks before layout-dependent uses.
//!   Calls forget owner-word checks while retaining the loaded value bounds.
//!   Owner guards keep their original load's eager frame and remain only
//!   when a consumer relies on the bound, including through a non-loop phi.
//! - Guarded method nodes retain positions in the canonical target vector.
//!   Targets without call linkage do not admit Graph calls or body inlining.
//! - A held method or a source with one closure context may fold watched
//!   context slots. Source and slot dependencies guard the respective facts;
//!   each slot store notifies its context's watch.
//!
//! # See also
//! - [`super::bytecode`] — blocks, loops and liveness.
//! - [`super::ir`] — the node vocabulary.

use otter_bytecode::Op;
use otter_bytecode::opcode_schema::ContextCoord;
use otter_vm::jit::{JitElementAccess, JitElementBase, JitElementRepr};
use otter_vm::native_abi::ExitReason;
use otter_vm::{JitCompileSnapshot, value::tag};
use rustc_hash::{FxHashMap, FxHashSet};
use smallvec::SmallVec;

use std::rc::Rc;
use std::sync::Arc;

use super::bytecode::{Analysis, Flow, Instruction, RegisterSet};
use super::ir::{
    BlockId, BranchKind, Condition, DeoptReason, FrameState, FrameStateId, Graph, InlineCaller,
    InlinedBody, Kind, NamedStorePlan, NodeId, Repr,
};
use crate::field_store::{FieldStoreOwner, StoreValueBound};

#[cfg(test)]
#[path = "builder_feedback_tests.rs"]
mod feedback_tests;

#[cfg(test)]
#[path = "builder_allocation_tests.rs"]
mod allocation_tests;

#[cfg(test)]
#[path = "builder_field_tests.rs"]
mod field_tests;

#[cfg(test)]
#[path = "builder_receiver_inline_tests.rs"]
mod receiver_inline_tests;

#[cfg(test)]
#[path = "builder_binding_tests.rs"]
mod binding_tests;

#[cfg(test)]
#[path = "builder_context_constant_tests.rs"]
mod context_constant_tests;

#[path = "builder_field_type.rs"]
mod field_type;
use field_type::FieldValueBound;

#[path = "builder_intrinsic_calls.rs"]
mod intrinsic_calls;

#[path = "builder_array_methods.rs"]
mod array_methods;

#[path = "builder_peel.rs"]
mod peel;
use peel::PeelPass;

const UNDEFINED: u64 = tag::VALUE_UNDEFINED;

/// What is known about one value along the current path that no heap write
/// changes.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct NodeInfo {
    /// The same exact Number as an unboxed int32, excluding -0. This does
    /// not prove that its Tagged form has the physical Int32 encoding.
    pub(crate) int32: Option<NodeId>,
    /// The same value as an unboxed double.
    pub(crate) float64: Option<NodeId>,
    /// The same value as a tagged word.
    pub(crate) tagged: Option<NodeId>,
    /// The value is proved to be an ECMAScript Object receiver, so ordinary
    /// sloppy `this` binding returns this same value without allocation.
    pub(crate) heap_object: bool,
    /// The value is proved to be a Number.
    pub(crate) number: bool,
    /// The value has an ordinary object body, by birth or a shape check.
    /// Its physical kind never changes, independently of mutable layout.
    pub(crate) ordinary_object: bool,
    /// The value has an Array body, by birth or a successful length check.
    /// Semantic Array proofs alone do not exclude Proxy bodies.
    pub(crate) array_body: bool,
}

/// What is known about one object's layout along the current path, until a
/// heap write may change it.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct LayoutInfo {
    /// Shapes the value is proved to have (an ordinary object with one of
    /// them).
    pub(crate) shapes: Option<SmallVec<[u32; 4]>>,
    /// The shape proof also excludes prototype objects.
    pub(crate) writable: bool,
    /// The value is proved an indexed receiver of this element layout.
    pub(crate) elements: Option<JitElementAccess>,
}

/// The nodes that read a proved indexed receiver's storage.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct ElementStorage {
    /// `Word` element count.
    pub(crate) length: NodeId,
    /// `Word` element base.
    pub(crate) base: NodeId,
}

/// A heap word whose current value a path knows: a context slot or a named
/// property slot of one object node, at a byte offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct FieldKey {
    object: NodeId,
    property: bool,
    offset: i32,
}

/// What one heap fact is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum FactKey {
    /// The layout facts of a node.
    Node(NodeId),
    /// The value of a heap word.
    Field(FieldKey),
}

/// One heap fact a loop header assumes holds on every iteration.
#[derive(Debug, Clone, PartialEq)]
enum HeapFact {
    Layout {
        shapes: Option<SmallVec<[u32; 4]>>,
        writable: bool,
        elements: Option<JitElementAccess>,
    },
    Field(NodeId),
}

/// One bytecode loop of the compilation: its header block in the body
/// it belongs to, the compiled function's own (`origin` 0) or an inlined
/// one (the body's `Graph::inlined` origin).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct LoopKey {
    origin: u16,
    header: usize,
}

/// How a loop header treats the heap facts of its entry.
#[derive(Debug, Clone, Default)]
struct LoopPolicy {
    /// The loop calls out: no heap fact survives an iteration.
    clobber: bool,
    /// Facts a back edge arrived without.
    dropped: FxHashSet<FactKey>,
}

/// Facts along the current path.
#[derive(Debug, Clone, Default)]
pub(crate) struct Known {
    info: FxHashMap<NodeId, NodeInfo>,
    /// Layout proofs, which every heap write may clobber.
    layouts: FxHashMap<NodeId, LayoutInfo>,
    /// The `Word` element count and element base of a proved indexed
    /// receiver, already read on this path; a collection may move them.
    storage: FxHashMap<NodeId, ElementStorage>,
    /// Heap words whose current value is a node on this path.
    fields: FxHashMap<FieldKey, NodeId>,
    /// The parent of each context loaded on this path. Context chains never
    /// change, so no heap write forgets these.
    parents: FxHashMap<NodeId, NodeId>,
    /// `(index, length)` pairs a bounds check proved on this path; both are
    /// immutable values, so no write forgets these.
    bounds: FxHashSet<(NodeId, NodeId)>,
    /// Elements read on this path: `(element base, index)` to the load and
    /// the value it read, until an element store or a call.
    elements_read: FxHashMap<(NodeId, NodeId), (Kind, NodeId)>,
    /// The compiled function's receiver, already loaded on this path. The
    /// activation's `this` binding never changes, so no write forgets it.
    this: Option<NodeId>,
    /// Cells a buffer bump allocated on this path since its last safepoint:
    /// the buffer serves only while no marking runs, so each is young and
    /// unmarked, and a store into it owes no barrier. Any call, collection
    /// point or poll forgets them.
    fresh: FxHashSet<NodeId>,
    /// Shape proofs an operation that may run code has since overtaken, of
    /// nodes whose every proved shape was stable when the generation
    /// compiled: still true for as long as those shapes stay stable, which
    /// a use confirms from the shapes before relying on them.
    stale_shapes: FxHashMap<NodeId, SmallVec<[u32; 4]>>,
    /// Shapes whose stability this path already proved since its last such
    /// operation.
    stable_checked: FxHashSet<u32>,
    /// Nullable value bounds; their target layouts require stability checks.
    field_values: FxHashMap<NodeId, FieldValueBound>,
    /// Defining field guards available since the last possible widening.
    checked_field_types: FxHashMap<u32, (u64, NodeId)>,
}

impl Known {
    fn get(&self, node: NodeId) -> Option<&NodeInfo> {
        self.info.get(&node)
    }

    fn entry(&mut self, node: NodeId) -> &mut NodeInfo {
        self.info.entry(node).or_default()
    }

    fn layout(&self, node: NodeId) -> Option<&LayoutInfo> {
        self.layouts.get(&node)
    }

    fn layout_entry(&mut self, node: NodeId) -> &mut LayoutInfo {
        self.layouts.entry(node).or_default()
    }

    /// Keep only facts `other` agrees on.
    fn intersect(&mut self, other: &Known) {
        self.info.retain(|node, info| {
            let Some(theirs) = other.info.get(node) else {
                return false;
            };
            if info.int32 != theirs.int32 {
                info.int32 = None;
            }
            if info.float64 != theirs.float64 {
                info.float64 = None;
            }
            if info.tagged != theirs.tagged {
                info.tagged = None;
            }
            info.heap_object &= theirs.heap_object;
            info.number &= theirs.number;
            info.ordinary_object &= theirs.ordinary_object;
            info.array_body &= theirs.array_body;
            *info != NodeInfo::default()
        });
        // A stale proof of stable shapes holds where the other edge proves
        // the same shapes outright.
        let fresh = |known: &Known, node: &NodeId| {
            known
                .layouts
                .get(node)
                .and_then(|layout| layout.shapes.clone())
        };
        let mut stale: FxHashMap<NodeId, SmallVec<[u32; 4]>> = FxHashMap::default();
        for (node, shapes) in &self.stale_shapes {
            if other.stale_shapes.get(node) == Some(shapes)
                || fresh(other, node).as_ref() == Some(shapes)
            {
                stale.insert(*node, shapes.clone());
            }
        }
        for (node, shapes) in &other.stale_shapes {
            if fresh(self, node).as_ref() == Some(shapes) {
                stale.insert(*node, shapes.clone());
            }
        }
        self.layouts.retain(|node, layout| {
            let Some(theirs) = other.layouts.get(node) else {
                return false;
            };
            if layout.shapes != theirs.shapes {
                layout.shapes = None;
            }
            // Paths that proved different strengths keep the weaker proof.
            layout.elements = match (layout.elements, theirs.elements) {
                (Some(ours), Some(other)) if ours.implies(&other) => Some(other),
                (Some(ours), Some(other)) if other.implies(&ours) => Some(ours),
                _ => None,
            };
            layout.writable &= theirs.writable;
            *layout != LayoutInfo::default()
        });
        self.storage
            .retain(|node, storage| other.storage.get(node) == Some(storage));
        self.fields
            .retain(|key, value| other.fields.get(key) == Some(value));
        self.parents
            .retain(|context, parent| other.parents.get(context) == Some(parent));
        self.bounds.retain(|pair| other.bounds.contains(pair));
        self.elements_read
            .retain(|key, read| other.elements_read.get(key) == Some(read));
        if self.this != other.this {
            self.this = None;
        }
        self.fresh.retain(|cell| other.fresh.contains(cell));
        self.stale_shapes = stale;
        self.stable_checked
            .retain(|shape| other.stable_checked.contains(shape));
        self.field_values.retain(|node, bound| {
            let Some(union) = other
                .field_values
                .get(node)
                .and_then(|other| bound.union(other))
            else {
                return false;
            };
            *bound = union;
            true
        });
        self.checked_field_types
            .retain(|owner, expected| other.checked_field_types.get(owner) == Some(expected));
    }

    /// Keep each layout proof whose shapes were all stable at compile time
    /// as a stale proof before an operation forgets it.
    fn keep_stable_shapes(&mut self, stable: &FxHashSet<u32>) {
        for (&node, layout) in &self.layouts {
            if let Some(shapes) = &layout.shapes
                && shapes.iter().all(|shape| stable.contains(shape))
            {
                self.stale_shapes.insert(node, shapes.clone());
            }
        }
        self.stable_checked.clear();
    }

    /// Forget every fact a heap write may invalidate; proofs of stable
    /// shapes stay as stale proofs.
    fn clobber_heap(&mut self, stable: &FxHashSet<u32>) {
        self.keep_stable_shapes(stable);
        self.layouts.clear();
        self.storage.clear();
        self.fields.clear();
        self.elements_read.clear();
        self.fresh.clear();
        self.checked_field_types.clear();
    }

    /// The value a store just wrote to `key`. A store through another node
    /// may write the same word, so every other value at its offset is
    /// forgotten.
    fn store_field(&mut self, key: FieldKey, value: NodeId) {
        self.fields
            .retain(|other, _| other.property != key.property || other.offset != key.offset);
        self.fields.insert(key, value);
    }

    /// Forget every node's shape proof: a published transition changes the
    /// shape of an object any node may name, but never a stable one's.
    fn forget_shapes(&mut self, stable: &FxHashSet<u32>) {
        self.keep_stable_shapes(stable);
        self.layouts.retain(|_, layout| {
            layout.shapes = None;
            layout.writable = false;
            layout.elements.is_some()
        });
    }

    /// Forget the shape proofs that name any shape in `left`, which a
    /// published transition took an object out of: a node proved to have
    /// another shape names another object.
    fn forget_shapes_from(&mut self, left: &[u32]) {
        for layout in self.layouts.values_mut() {
            if layout
                .shapes
                .as_ref()
                .is_some_and(|shapes| shapes.iter().any(|shape| left.contains(shape)))
            {
                layout.shapes = None;
                layout.writable = false;
            }
        }
        self.stale_shapes
            .retain(|_, shapes| !shapes.iter().any(|shape| left.contains(shape)));
    }

    /// Forget every known value at `offset`: a store through any node may
    /// write it.
    fn forget_field_offset(&mut self, property: bool, offset: i32) {
        self.fields
            .retain(|key, _| key.property != property || key.offset != offset);
    }

    /// Forget the heap facts `keys` name.
    fn forget(&mut self, keys: &FxHashSet<FactKey>) {
        for key in keys {
            match *key {
                FactKey::Node(node) => {
                    self.layouts.remove(&node);
                    self.storage.remove(&node);
                }
                FactKey::Field(field) => {
                    self.fields.remove(&field);
                }
            }
        }
    }

    /// Forget what a collection invalidates: the storage words read on this
    /// path, which a scavenge may move, and the cells still known young.
    /// Layouts, field values and shape facts name nodes, which the collector
    /// keeps current, so they stay. A loop back edge polls for interrupts,
    /// and a collection there may run too.
    fn collect(&mut self) {
        self.storage.clear();
        self.elements_read.clear();
        self.fresh.clear();
    }

    /// Keep only the heap facts about `roots` and the values their known
    /// fields hold, transitively.
    fn retain_reachable(&mut self, roots: impl Iterator<Item = NodeId>) {
        let mut reachable: FxHashSet<NodeId> = roots.collect();
        loop {
            let before = reachable.len();
            for (key, &value) in &self.fields {
                if reachable.contains(&key.object) {
                    reachable.insert(value);
                }
            }
            if reachable.len() == before {
                break;
            }
        }
        self.layouts.retain(|node, _| reachable.contains(node));
        self.storage.retain(|node, _| reachable.contains(node));
        self.fields.retain(|key, _| reachable.contains(&key.object));
        self.stale_shapes.retain(|node, _| reachable.contains(node));
    }

    /// The heap facts on this path.
    fn heap_facts(&self) -> Vec<(FactKey, HeapFact)> {
        let mut facts: Vec<(FactKey, HeapFact)> = self
            .layouts
            .iter()
            .filter(|(_, layout)| layout.shapes.is_some() || layout.elements.is_some())
            .map(|(&node, layout)| {
                (
                    FactKey::Node(node),
                    HeapFact::Layout {
                        shapes: layout.shapes.clone(),
                        writable: layout.writable,
                        elements: layout.elements,
                    },
                )
            })
            .chain(
                self.fields
                    .iter()
                    .map(|(&key, &value)| (FactKey::Field(key), HeapFact::Field(value))),
            )
            .collect();
        facts.sort_by_key(|(key, _)| *key);
        facts
    }

    /// The keys of `facts` that no longer hold on this path.
    fn broken(&self, facts: &[(FactKey, HeapFact)]) -> impl Iterator<Item = FactKey> {
        facts
            .iter()
            .filter(|(key, fact)| match (key, fact) {
                (
                    FactKey::Node(node),
                    HeapFact::Layout {
                        shapes,
                        writable,
                        elements,
                    },
                ) => !self.layouts.get(node).is_some_and(|layout| {
                    (shapes.is_none() || layout.shapes == *shapes)
                        && (!writable || layout.writable)
                        && elements.is_none_or(|wanted| {
                            layout
                                .elements
                                .is_some_and(|proved| proved.implies(&wanted))
                        })
                }),
                (FactKey::Field(field), HeapFact::Field(value)) => {
                    self.fields.get(field) != Some(value)
                }
                _ => true,
            })
            .map(|(key, _)| *key)
            .collect::<Vec<_>>()
            .into_iter()
    }
}

/// One incoming edge's state at a join.
#[derive(Debug, Clone)]
struct Incoming {
    block: BlockId,
    frame: Vec<NodeId>,
    known: Known,
}

/// Why graph construction stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BuildError {
    /// The OSR PC does not start a reachable block.
    OsrTarget,
}

/// A finished graph plus the facts later phases need.
#[derive(Debug)]
pub(crate) struct Built {
    pub(crate) graph: Graph,
    /// Blocks in emission order.
    pub(crate) layout: Vec<BlockId>,
    /// The block an OSR entry jumps to; it reads every register live at the
    /// loop header from the interpreter's window.
    pub(crate) osr_entry: Option<BlockId>,
    /// Every loop header with a back edge.
    pub(crate) loop_headers: Vec<LoopHeader>,
    /// The snapshot of each inlined body, by origin minus one.
    pub(crate) inline_views: Vec<Arc<JitCompileSnapshot>>,
}

/// One loop header, for representation selection of its phis.
#[derive(Debug, Clone)]
pub(crate) struct LoopHeader {
    pub(crate) block: BlockId,
    /// The header's interpreter state with its back edge's values. An entry
    /// edge's state is this one with each phi register bound to that edge's
    /// input instead.
    pub(crate) state: FrameStateId,
    /// The header's phis by interpreter register.
    pub(crate) phis: Vec<(u16, NodeId)>,
    /// Whether an entry value may be speculated to be an int32: no earlier
    /// optimized code left at the header for a type mismatch.
    pub(crate) speculate: bool,
    /// Whether invariant shape checks may move out of the loop: no earlier
    /// optimized code left at the header for a shape or layout mismatch.
    pub(crate) hoist: bool,
    /// Every reason earlier optimized code left at the header for, where a
    /// check moved out of the loop leaves.
    pub(crate) exit_reasons: SmallVec<[ExitReason; 4]>,
}

struct Builder<'a> {
    /// The compiled function's own snapshot.
    root: &'a JitCompileSnapshot,
    /// The snapshot of the body being visited.
    view: &'a JitCompileSnapshot,
    analysis: Rc<Analysis>,
    baseline: &'a super::BaselineSupport,
    /// The strict tail call being lowered as an ordinary call: a fallback
    /// to the baseline operation runs it, not the call it was rewritten to.
    tail_call: Option<Instruction>,
    /// Rest lists of spliced bodies, built from known values, with the node
    /// count after each was built: one nothing has written since still holds
    /// those values.
    pristine_lists: Vec<(NodeId, usize)>,
    graph: Graph,
    function_id: u32,
    register_count: u16,
    /// Graph block per bytecode block.
    block_map: Vec<BlockId>,
    /// Split blocks appended after a bytecode block in emission order.
    trailing: Vec<Vec<BlockId>>,
    incoming: Vec<Vec<Incoming>>,
    /// Loop-header phis per bytecode header: `(register, phi)`.
    loop_phis: FxHashMap<usize, Vec<(u16, NodeId)>>,
    current: Option<BlockId>,
    current_bytecode_block: usize,
    /// The slot of `block_map`/`incoming`/`trailing` the current block
    /// occupies: its bytecode block, or that block's peeled copy.
    current_slot: usize,
    /// The loop whose first iteration is being built ahead of it.
    peel: Option<PeelPass>,
    /// The loops of this body built with a peeled first iteration, by header.
    peel_plans: Rc<FxHashMap<usize, peel::PeelPlan>>,
    frame: Vec<NodeId>,
    known: Known,
    /// The bytecode loop header the OSR entry block feeds.
    osr_header: Option<usize>,
    /// Eager frame states already built, by PC.
    eager_states: FxHashMap<u32, FrameStateId>,
    /// How each loop header treats the heap facts of its entry.
    policies: &'a FxHashMap<LoopKey, LoopPolicy>,
    /// Heap facts each loop header assumes for its whole body.
    assumptions: FxHashMap<LoopKey, Vec<(FactKey, HeapFact)>>,
    /// Loops whose body may write the heap through a call.
    effectful: FxHashSet<LoopKey>,
    /// Per loop header, the assumed facts a back edge arrived without.
    failed: FxHashMap<LoopKey, FxHashSet<FactKey>>,
    /// The loops each bytecode block lies in: its own body's and, in an
    /// inlined body, every loop around the call it runs for.
    enclosing: Vec<SmallVec<[LoopKey; 2]>>,
    /// Finished loops of the inlined bodies built so far.
    inline_loop_headers: Vec<LoopHeader>,
    pc: u32,
    undefined: NodeId,
    /// The inlined call the body being visited runs for; `None` in the
    /// compiled function's own body.
    inline: Option<Box<InlineFrame>>,
    /// The prepared bodies this compilation builds in place of their calls.
    inline_plan: super::inline_plan::InlinePlan,
    /// The snapshot of each inlined body, by origin minus one.
    inline_views: Vec<Arc<JitCompileSnapshot>>,
    /// Owner guards required by a consumed value bound.
    required_field_guards: FxHashSet<NodeId>,
    /// Nullish comparisons whose ordinary arms depend on owner guards.
    field_nullish_candidates: FxHashMap<NodeId, SmallVec<[NodeId; 4]>>,
    /// Contexts of the closure an inlined body was baked against, by node:
    /// that body's snapshot and the parent hops from the closure's context.
    closure_contexts: FxHashMap<NodeId, (*const JitCompileSnapshot, u16)>,
    /// Constant context-slot reads of a closure, by node: its function id.
    /// The slot's dependency keeps that closure the value for the code's
    /// lifetime.
    constant_closures: FxHashMap<NodeId, u32>,
    /// The identity cell of the own-property bag of each constant closure
    /// that had one at the bake.
    constant_bags: FxHashMap<NodeId, u64>,
}

/// The most actual arguments a spliced body's elided `arguments[i]` selects
/// among; a call with more keeps the body out of line.
const MAX_SELECTED_ARGUMENTS: usize = 8;

/// How an inlined body binds `this`.
#[derive(Clone, Copy)]
enum InlineThis {
    /// A call's actual receiver.
    Bound(NodeId),
    /// A construct's receiver, allocated in place as this plan describes,
    /// with this new.target: the constructed closure itself, or the class
    /// wrapper whose constructor body runs.
    Construct(otter_vm::jit::JitReceiverAllocationPlan, NodeId),
}

/// The call an inlined body runs for.
struct InlineFrame {
    /// The caller state and entry bindings every frame state of the body
    /// links to.
    caller: InlineCaller,
    depth: u8,
    /// The caller's block the body's returns jump to.
    continuation: BlockId,
    /// Each return: its block, its value and the facts that hold there.
    returns: Vec<(BlockId, NodeId, Known)>,
    /// The receiver of an inlined `[[Construct]]`: the result of every
    /// return of a value that is not an Object.
    receiver: Option<NodeId>,
    /// The body needs something only an activation of its own provides.
    abandoned: bool,
    /// The callee is a heap closure a proven method slot fixes, never a
    /// bare function id.
    heap_closure: bool,
}

/// The half of the builder that describes the body being visited. Inlining
/// swaps in the callee's, and the caller's back once the callee is built.
struct Scope<'a> {
    view: &'a JitCompileSnapshot,
    analysis: Rc<Analysis>,
    function_id: u32,
    register_count: u16,
    block_map: Vec<BlockId>,
    trailing: Vec<Vec<BlockId>>,
    incoming: Vec<Vec<Incoming>>,
    loop_phis: FxHashMap<usize, Vec<(u16, NodeId)>>,
    current_bytecode_block: usize,
    current_slot: usize,
    peel: Option<PeelPass>,
    peel_plans: Rc<FxHashMap<usize, peel::PeelPlan>>,
    frame: Vec<NodeId>,
    eager_states: FxHashMap<u32, FrameStateId>,
    enclosing: Vec<SmallVec<[LoopKey; 2]>>,
    pc: u32,
    inline: Option<Box<InlineFrame>>,
}

/// Build the graph of `view`.
///
/// Loop headers keep the heap facts of their entry optimistically. A build
/// in which a back edge arrives without one of them is redone with every
/// loop that calls out, and every loop that failed, entered without heap
/// facts.
pub(crate) fn build(
    view: &JitCompileSnapshot,
    analysis: &Rc<Analysis>,
    baseline: &'_ super::BaselineSupport,
    osr_pc: Option<u32>,
) -> Result<Built, BuildError> {
    let mut policies: FxHashMap<LoopKey, LoopPolicy> = FxHashMap::default();
    loop {
        let BuildPass {
            built,
            effectful,
            failed,
        } = build_once(view, analysis, baseline, osr_pc, &policies)?;
        if failed.is_empty() {
            return Ok(built);
        }
        for (header, keys) in failed {
            let policy = policies.entry(header).or_default();
            if effectful.contains(&header) {
                policy.clobber = true;
            } else {
                debug_assert!(
                    !keys.is_subset(&policy.dropped),
                    "a dropped fact never fails"
                );
                policy.dropped.extend(keys);
            }
        }
    }
}

/// One construction of the graph under a set of loop policies.
struct BuildPass {
    built: Built,
    /// Loops whose body calls out.
    effectful: FxHashSet<LoopKey>,
    /// Per loop header, the assumed facts a back edge arrived without.
    failed: FxHashMap<LoopKey, FxHashSet<FactKey>>,
}

fn build_once(
    view: &JitCompileSnapshot,
    analysis: &Rc<Analysis>,
    baseline: &'_ super::BaselineSupport,
    osr_pc: Option<u32>,
    policies: &FxHashMap<LoopKey, LoopPolicy>,
) -> Result<BuildPass, BuildError> {
    let code = view.code_block.as_ref();
    let register_count = code.register_count;
    let mut graph = Graph::default();
    let undefined = graph.constant(Kind::ConstTagged(UNDEFINED));
    let block_count = analysis.blocks.len();
    let peel_plans = peel::plans(view, analysis);
    // Each block has its own slot; a peeled loop's blocks also have a
    // second one, offset by the block count, for their first iteration.
    let mut block_map = vec![BlockId(u32::MAX); 2 * block_count];
    for &block in &analysis.order {
        block_map[block] = graph.new_block();
    }
    for plan in peel_plans.values() {
        for &block in plan.body.iter() {
            block_map[block + block_count] = graph.new_block();
        }
    }
    let mut builder = Builder {
        root: view,
        view,
        analysis: analysis.clone(),
        baseline,
        tail_call: None,
        pristine_lists: Vec::new(),
        graph,
        function_id: code.id,
        register_count,
        block_map,
        trailing: vec![Vec::new(); 2 * block_count],
        incoming: vec![Vec::new(); 2 * block_count],
        loop_phis: FxHashMap::default(),
        current: None,
        current_bytecode_block: 0,
        current_slot: usize::MAX,
        peel: None,
        peel_plans: Rc::new(peel_plans),
        frame: vec![undefined; usize::from(register_count)],
        known: Known::default(),
        osr_header: osr_pc.and_then(|pc| analysis.block_of.get(pc as usize).copied()),
        eager_states: FxHashMap::default(),
        policies,
        assumptions: FxHashMap::default(),
        effectful: FxHashSet::default(),
        failed: FxHashMap::default(),
        enclosing: loop_nesting(analysis, 0, &[]),
        inline_loop_headers: Vec::new(),
        pc: 0,
        undefined,
        inline: None,
        inline_plan: super::inline_plan::InlinePlan::choose(view),
        inline_views: Vec::new(),
        required_field_guards: FxHashSet::default(),
        field_nullish_candidates: FxHashMap::default(),
        closure_contexts: FxHashMap::default(),
        constant_closures: FxHashMap::default(),
        constant_bags: FxHashMap::default(),
    };
    let entry = builder.graph.new_block();
    builder.start_entry(entry);
    let mut layout = vec![entry];
    let mut osr_entry = None;
    if let Some(pc) = osr_pc {
        let header = *analysis
            .block_of
            .get(pc as usize)
            .ok_or(BuildError::OsrTarget)?;
        if analysis.blocks[header].start != pc || !analysis.blocks[header].reachable {
            return Err(BuildError::OsrTarget);
        }
        let block = builder.graph.new_block();
        builder.start_osr_entry(block, header);
        layout.push(block);
        osr_entry = Some(block);
    }
    layout.extend(builder.build_body());
    builder.finish_loops();
    builder.prune_field_guards();
    remove_trivial_phis(&mut builder.graph, &layout);
    let mut loop_headers = builder.loop_headers();
    loop_headers.append(&mut builder.inline_loop_headers);
    loop_headers.sort_by_key(|header| header.block);
    Ok(BuildPass {
        built: Built {
            graph: builder.graph,
            layout,
            osr_entry,
            loop_headers,
            inline_views: builder.inline_views,
        },
        effectful: builder.effectful,
        failed: builder.failed,
    })
}

/// The loops each bytecode block of a body lies in: the body's own loops,
/// keyed by its `origin`, then `outer`, every loop around the call an
/// inlined body runs for.
fn loop_nesting(
    analysis: &Analysis,
    origin: u16,
    outer: &[LoopKey],
) -> Vec<SmallVec<[LoopKey; 2]>> {
    let mut enclosing: Vec<SmallVec<[LoopKey; 2]>> =
        vec![outer.iter().copied().collect(); analysis.blocks.len()];
    for (&header, info) in &analysis.loops {
        for &block in &info.body {
            enclosing[block].push(LoopKey { origin, header });
        }
    }
    enclosing
}

/// The BigInt operator a binary opcode names.
fn bigint_operator(op: Op) -> Option<otter_vm::bigint::ops::Operator> {
    use otter_vm::bigint::ops::Operator;
    Some(match op {
        Op::Add => Operator::Add,
        Op::Sub => Operator::Sub,
        Op::Mul => Operator::Mul,
        Op::Div => Operator::Div,
        Op::Rem => Operator::Rem,
        Op::BitwiseAnd => Operator::BitwiseAnd,
        Op::BitwiseOr => Operator::BitwiseOr,
        Op::BitwiseXor => Operator::BitwiseXor,
        Op::Shl => Operator::Shl,
        Op::Shr => Operator::Shr,
        _ => return None,
    })
}

/// Replace every phi that merges one value with that value.
///
/// A phi whose inputs, apart from references to itself, are all the same
/// node `v` is `v`: the only value that reaches it is `v`, which therefore
/// dominates it. Replacing one phi can make the phis that read it trivial in
/// turn, so the pass runs to a fixpoint, then rewrites node inputs, phi
/// inputs and frame states and drops the replaced phis.
pub(crate) fn remove_trivial_phis(graph: &mut Graph, layout: &[BlockId]) {
    fn resolve(replaced: &mut FxHashMap<NodeId, NodeId>, node: NodeId) -> NodeId {
        let mut current = node;
        while let Some(&next) = replaced.get(&current) {
            current = next;
        }
        // Compress the chain behind `node`.
        let mut walk = node;
        while let Some(&next) = replaced.get(&walk) {
            if next == current {
                break;
            }
            replaced.insert(walk, current);
            walk = next;
        }
        current
    }
    let phis: Vec<NodeId> = layout
        .iter()
        .flat_map(|&block| graph.block(block).phis.iter().copied())
        .collect();
    let mut replaced: FxHashMap<NodeId, NodeId> = FxHashMap::default();
    loop {
        let mut changed = false;
        for &phi in &phis {
            if replaced.contains_key(&phi) {
                continue;
            }
            let mut same = None;
            let mut trivial = true;
            for index in 0..graph.node(phi).inputs.len() {
                let input = resolve(&mut replaced, graph.node(phi).inputs[index]);
                if input == phi || Some(input) == same {
                    continue;
                }
                if same.is_some() {
                    trivial = false;
                    break;
                }
                same = Some(input);
            }
            if trivial && let Some(value) = same {
                replaced.insert(phi, value);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    if replaced.is_empty() {
        return;
    }
    let keys: Vec<NodeId> = replaced.keys().copied().collect();
    for key in keys {
        resolve(&mut replaced, key);
    }
    for node in &mut graph.nodes {
        for input in &mut node.inputs {
            if let Some(&value) = replaced.get(input) {
                *input = value;
            }
        }
    }
    for state in &mut graph.frame_states {
        state.for_each_value_mut(|value| {
            if let Some(&value_after) = replaced.get(value) {
                *value = value_after;
            }
        });
    }
    for &block in layout {
        graph
            .block_mut(block)
            .phis
            .retain(|phi| !replaced.contains_key(phi));
    }
}

impl<'a> Builder<'a> {
    /// Visit every reachable block of the current body in reverse
    /// post-order. Returns the body's blocks in emission order.
    fn build_body(&mut self) -> Vec<BlockId> {
        let analysis = self.analysis.clone();
        let mut layout = Vec::new();
        for &block in &analysis.order {
            if self.inline.as_ref().is_some_and(|inline| inline.abandoned) {
                break;
            }
            if let Some(plan) = self.peel_plans.get(&block).cloned() {
                self.build_peeled_iteration(block, &plan, &mut layout);
            }
            self.build_block(block, block, &mut layout);
        }
        layout
    }

    /// Build bytecode block `block` into `slot`, appending its graph
    /// blocks to `layout`; nothing when no edge reaches it.
    fn build_block(&mut self, block: usize, slot: usize, layout: &mut Vec<BlockId>) {
        let analysis = self.analysis.clone();
        if !self.start_block(block, slot) {
            return;
        }
        layout.push(self.block_map[slot]);
        let range = analysis.blocks[block].start..analysis.blocks[block].end;
        for pc in range.clone() {
            self.pc = pc;
            self.graph.position = pc;
            self.visit(&analysis.instructions[pc as usize]);
            if self.current.is_none() {
                break;
            }
        }
        if self.current.is_some() {
            // Fell off the end of the block into its single successor.
            let last = &analysis.instructions[range.end as usize - 1];
            self.pc = last.pc;
            match analysis.blocks[block].successors.first() {
                Some(&successor) => self.goto(successor),
                None => self.deopt(DeoptReason::Unsupported),
            }
        }
        layout.extend(self.trailing[slot].iter().copied());
    }

    fn swap_scope(&mut self, scope: &mut Scope<'a>) {
        use std::mem::swap;
        swap(&mut self.view, &mut scope.view);
        swap(&mut self.analysis, &mut scope.analysis);
        swap(&mut self.function_id, &mut scope.function_id);
        swap(&mut self.register_count, &mut scope.register_count);
        swap(&mut self.block_map, &mut scope.block_map);
        swap(&mut self.trailing, &mut scope.trailing);
        swap(&mut self.incoming, &mut scope.incoming);
        swap(&mut self.loop_phis, &mut scope.loop_phis);
        swap(
            &mut self.current_bytecode_block,
            &mut scope.current_bytecode_block,
        );
        swap(&mut self.current_slot, &mut scope.current_slot);
        swap(&mut self.peel, &mut scope.peel);
        swap(&mut self.peel_plans, &mut scope.peel_plans);
        swap(&mut self.frame, &mut scope.frame);
        swap(&mut self.eager_states, &mut scope.eager_states);
        swap(&mut self.enclosing, &mut scope.enclosing);
        swap(&mut self.pc, &mut scope.pc);
        swap(&mut self.inline, &mut scope.inline);
    }

    /// Whether `body` may be built in place of a call from the current
    /// body: a function without handlers the inline plan chose, within the
    /// size and depth bounds (deeper for a small body), called from outside
    /// any exception region of the compiled function.
    fn inlinable(&self, body: &JitCompileSnapshot) -> bool {
        let code = body.code_block.as_ref();
        let instructions = code.instruction_count();
        let depth = self.inline.as_ref().map_or(0, |inline| inline.depth);
        let outer_pc = self.graph.outer_pc_at(self.graph.origin, self.pc);
        // A small body costs less inlined than its call does, so it is
        // inlined deeper.
        let within_depth = if instructions <= otter_vm::jit::JIT_SMALL_INLINE_INSTRUCTIONS {
            depth < otter_vm::jit::JIT_SMALL_INLINE_DEPTH
        } else {
            depth < otter_vm::jit::JIT_INLINE_DEPTH
        };
        within_depth
            && self.inline_recursion(code.id) < otter_vm::jit::JIT_INLINE_RECURSION
            && self.inline_plan.admits(body)
            && code.admits_graph_inlining()
            && self.graph.inlined.len() < usize::from(u16::MAX)
            && self.root_handler_at(outer_pc).is_none()
    }

    /// How many bodies on the current inline path, the compiled function
    /// included, run `function_id`.
    fn inline_recursion(&self, function_id: u32) -> usize {
        let mut count = usize::from(self.root.code_block.id == function_id);
        let mut origin = self.graph.origin;
        while origin != 0 {
            let body = self.graph.inlined[usize::from(origin) - 1];
            count += usize::from(body.function_id == function_id);
            origin = body.parent;
        }
        count
    }

    /// The exception handler of the compiled function covering `pc`.
    fn root_handler_at(&self, pc: u32) -> Option<otter_bytecode::ExceptionHandler> {
        self.root.code_block.control_flow().handler_at(pc)
    }

    /// Build `body` in place of the call at the current instruction, whose
    /// callee is proved to be `closure` and whose activation would bind
    /// `this` and receive `arguments`. The callee's returns meet in a fresh
    /// block that continues the caller with the returned value in the
    /// call's register. Returns `false`, with the graph as it was, when the
    /// body needs an activation of its own.
    fn inline_call(
        &mut self,
        instruction: &Instruction,
        body: &'a Arc<JitCompileSnapshot>,
        plan: otter_vm::jit::JitDirectCallPlan,
        closure: NodeId,
        this: NodeId,
        arguments: &[NodeId],
    ) -> bool {
        let this = InlineThis::Bound(this);
        self.inline_frame(instruction, body, plan, closure, this, arguments)
    }

    /// Build `body` in place of the `new` at the current instruction, whose
    /// callee is proved to be `closure` and is its own new.target: the
    /// receiver `allocation` describes is allocated in place, bound as
    /// `this`, and is the result unless the body returns an Object.
    fn inline_construct(
        &mut self,
        instruction: &Instruction,
        body: &'a Arc<JitCompileSnapshot>,
        plan: otter_vm::jit::JitDirectCallPlan,
        allocation: otter_vm::jit::JitReceiverAllocationPlan,
        closure: NodeId,
        new_target: NodeId,
        arguments: &[NodeId],
    ) -> bool {
        let this = InlineThis::Construct(allocation, new_target);
        self.inline_frame(instruction, body, plan, closure, this, arguments)
    }

    /// Whether `closure` is a guarded method read of this body whose holder
    /// slot the prototype proof fixes to one heap closure.
    /// Whether `function` is a constant context-slot closure of the body
    /// `plan` enters: a call of it needs no identity speculation, so a site
    /// whose other identity proofs left still calls it directly.
    fn constant_closure_of(
        &self,
        function: NodeId,
        plan: otter_vm::jit::JitDirectCallPlan,
    ) -> bool {
        self.constant_closures.get(&function) == Some(&plan.function_id)
    }

    fn proven_heap_closure(&self, closure: NodeId) -> bool {
        if self.constant_closures.contains_key(&closure) {
            return true;
        }
        let Kind::LoadGuardedMethod {
            byte_pc, target, ..
        } = self.graph.node(closure).kind
        else {
            return false;
        };
        self.view
            .method_targets
            .get(&byte_pc)
            .and_then(|methods| methods.get(usize::from(target)))
            .is_some_and(|method| {
                method.guard.slot_proven
                    && method.guard.prototype_validity.is_some()
                    && method.guard.method_cell != 0
            })
    }

    /// Whether `closure` is a guarded method read whose target's prototype
    /// proof fixes the holder slot to a closure of `function_id` (see
    /// [`otter_vm::jit::JitMethodGuard::slot_proven`]): the read's own guard
    /// already proved what a function check would.
    fn proven_method_of(&self, closure: NodeId, function_id: u32) -> bool {
        if self.constant_closures.get(&closure) == Some(&function_id) {
            return true;
        }
        let Kind::LoadGuardedMethod {
            byte_pc, target, ..
        } = self.graph.node(closure).kind
        else {
            return false;
        };
        self.view
            .method_targets
            .get(&byte_pc)
            .and_then(|methods| methods.get(usize::from(target)))
            .is_some_and(|method| {
                method.guard.slot_proven
                    && method.guard.prototype_validity.is_some()
                    && method.guard.method_fid == function_id
            })
    }

    /// The shared body of [`Self::inline_call`] and
    /// [`Self::inline_construct`].
    fn inline_frame(
        &mut self,
        instruction: &Instruction,
        body: &'a Arc<JitCompileSnapshot>,
        plan: otter_vm::jit::JitDirectCallPlan,
        closure: NodeId,
        this: InlineThis,
        arguments: &[NodeId],
    ) -> bool {
        let cell = plan.callee_cell;
        let heap_closure = self.proven_heap_closure(closure);
        let Some(block) = self.current else {
            return false;
        };
        if !self.inlinable(body) {
            return false;
        }
        let Ok(analysis) = Analysis::build(body) else {
            return false;
        };
        let code = body.code_block.as_ref();
        let checkpoint = self.graph.checkpoint(block);
        let frame_states = self.graph.frame_states.len();
        let views = self.inline_views.len();
        let known = self.known.clone();
        let required_field_guards = self.required_field_guards.clone();
        let field_nullish_candidates = self.field_nullish_candidates.clone();
        // The callee runs only as the function the body is. A method read
        // from a slot its prototype proof fixes is already a closure of that
        // function.
        if !self.proven_method_of(closure, code.id) {
            self.add(
                Kind::CheckFunction {
                    function_id: code.id,
                    cell,
                },
                &[closure],
                Repr::None,
            );
        }
        // A construct's receiver exists before its body runs; a failed proof
        // or refill resumes the whole construct.
        let (this, new_target, receiver) = match this {
            InlineThis::Bound(this) => (this, self.undefined, None),
            InlineThis::Construct(allocation, new_target) => {
                let receiver = self.add(Kind::NewReceiver(allocation), &[new_target], Repr::Tagged);
                let shape = match allocation.receiver_shape {
                    0 => allocation.prototype_root,
                    shape => shape,
                };
                let layout = self.known.layout_entry(receiver);
                layout.shapes = Some(smallvec::smallvec![shape]);
                layout.writable = true;
                self.known.entry(receiver).heap_object = true;
                // A failed bump resumes the construct elsewhere: the node only
                // ever yields a buffer cell.
                self.known.fresh.insert(receiver);
                (receiver, new_target, Some(receiver))
            }
        };
        let destination = instruction.writes[0];
        // The caller resumes after the call; the call's register is written
        // by the return, so its old value is never restored.
        let caller_state = self.lazy_state(Some((destination, self.undefined)));
        let caller_origin = self.graph.origin;
        self.graph.inlined.push(InlinedBody {
            function_id: code.id,
            parent: caller_origin,
            call_pc: self.pc,
            call_byte_pc: instruction.byte_pc,
        });
        self.inline_views.push(body.clone());
        let origin = self.graph.inlined.len() as u16;
        let continuation = self.graph.new_block();
        let loops = self
            .enclosing
            .get(self.current_bytecode_block)
            .cloned()
            .unwrap_or_default();
        let depth = self.inline.as_ref().map_or(0, |inline| inline.depth) + 1;
        let block_count = analysis.blocks.len();
        let mut block_map = vec![BlockId(u32::MAX); block_count];
        for &callee_block in &analysis.order {
            block_map[callee_block] = self.graph.new_block();
        }
        let register_count = code.register_count;
        let mut frame = vec![self.undefined; usize::from(register_count)];
        let params = usize::from(code.param_count.min(register_count));
        for (slot, &argument) in frame.iter_mut().zip(arguments).take(params) {
            *slot = argument;
        }
        let enclosing = loop_nesting(&analysis, origin, &loops);
        let mut scope = Scope {
            view: body,
            analysis: Rc::new(analysis),
            function_id: code.id,
            register_count,
            block_map,
            trailing: vec![Vec::new(); block_count],
            incoming: vec![Vec::new(); block_count],
            loop_phis: FxHashMap::default(),
            current_bytecode_block: usize::MAX,
            current_slot: usize::MAX,
            peel: None,
            peel_plans: Rc::default(),
            frame,
            eager_states: FxHashMap::default(),
            enclosing,
            pc: 0,
            inline: Some(Box::new(InlineFrame {
                caller: InlineCaller {
                    state: caller_state,
                    return_register: destination,
                    this,
                    closure,
                    new_target,
                    arguments: (code.exposes_legacy_arguments()
                        || code.needs_arguments()
                        || code.forwards_arguments()
                        || code.takes_rest())
                    .then(|| arguments.iter().copied().collect()),
                },
                depth,
                continuation,
                returns: Vec::new(),
                receiver,
                abandoned: false,
                heap_closure,
            })),
        };
        let caller_position = self.graph.position;
        self.swap_scope(&mut scope);
        self.graph.origin = origin;
        self.graph.position = 0;
        // Loop headers recorded from here on belong to this body or to bodies
        // spliced into it; an abandoned body drops all of them.
        let inline_loop_headers = self.inline_loop_headers.len();
        // The caller's block jumps to the callee's first block.
        self.goto(0);
        let layout = self.build_body();
        if !self.inline.as_ref().is_some_and(|inline| inline.abandoned) {
            self.finish_loops();
            let mut headers = self.loop_headers();
            self.inline_loop_headers.append(&mut headers);
        }
        self.swap_scope(&mut scope);
        self.graph.origin = caller_origin;
        self.graph.position = caller_position;
        let frame = scope.inline.take().expect("the callee's inline frame");
        if frame.abandoned {
            // Loops of the abandoned body, and of bodies spliced into it,
            // go with it; their origins are reused.
            self.inline_loop_headers.truncate(inline_loop_headers);
            let kept = |key: &LoopKey| key.origin < origin;
            self.assumptions.retain(|key, _| kept(key));
            self.effectful.retain(kept);
            self.failed.retain(|key, _| kept(key));
            self.graph.rollback(checkpoint);
            self.inline_views.truncate(views);
            // States built since the checkpoint are gone; their ids return.
            self.eager_states
                .retain(|_, state| (state.0 as usize) < frame_states);
            self.known = known;
            self.required_field_guards = required_field_guards;
            self.field_nullish_candidates = field_nullish_candidates;
            self.current = Some(block);
            return false;
        }
        let owner = self.current_slot;
        self.trailing[owner].extend(layout);
        let mut returns = frame.returns;
        if returns.is_empty() {
            // Every path through the body leaves optimized code.
            self.current = None;
            return true;
        }
        self.trailing[owner].push(continuation);
        self.graph.block_mut(continuation).predecessors =
            returns.iter().map(|&(block, _, _)| block).collect();
        let mut known = returns[0].2.clone();
        for (_, _, edge) in &returns[1..] {
            known.intersect(edge);
        }
        self.current = Some(continuation);
        let bound = field_type::merged_bound(
            &self.graph,
            &self.root.stable_shapes,
            returns.iter().map(|(_, value, known)| (*value, known)),
        );
        let value = if returns.len() == 1 {
            returns[0].1
        } else {
            let inputs: SmallVec<[NodeId; 4]> = returns
                .iter_mut()
                .map(|(block, value, edge)| self.tagged_at_end(*block, *value, edge))
                .collect();
            let phi = self.graph.add_node(Kind::Phi, &inputs, Repr::Tagged);
            self.graph.node_mut(phi).block = Some(continuation);
            self.graph.block_mut(continuation).phis.push(phi);
            phi
        };
        self.known = known;
        if let Some(bound) = bound {
            self.known.field_values.insert(value, bound);
        }
        self.write(destination, value);
        true
    }

    /// Whether `value` is an ECMAScript Object along this path, when the
    /// graph proves it either way.
    /// Whether `value` is proved an ordinary object, whose cell kind no
    /// write changes.
    fn is_ordinary_object(&self, value: NodeId) -> bool {
        self.known
            .get(value)
            .is_some_and(|info| info.ordinary_object)
            || self
                .known
                .layout(value)
                .is_some_and(|layout| layout.shapes.is_some())
            || self.known.stale_shapes.contains_key(&value)
    }

    fn is_object(&self, value: NodeId) -> Option<bool> {
        if self.known.get(value).is_some_and(|info| info.heap_object) {
            return Some(true);
        }
        match self.graph.node(value).kind {
            Kind::ConstTagged(bits) => Some(otter_vm::Value::from_bits(bits).as_object().is_some()),
            _ if self.graph.node(value).repr != Repr::Tagged => Some(false),
            _ => None,
        }
    }

    /// The tagged form of `value` at the end of `block`, before its control
    /// node, as the facts `known` of that block provide or a box placed
    /// there.
    fn tagged_at_end(&mut self, block: BlockId, value: NodeId, known: &mut Known) -> NodeId {
        let node = self.graph.node(value);
        if node.repr == Repr::Tagged {
            return value;
        }
        if let Some(tagged) = known.get(value).and_then(|info| info.tagged) {
            return tagged;
        }
        let constant = match node.kind {
            Kind::ConstInt32(int) => Some(tag::NUMBER_TAG | u64::from(int as u32)),
            Kind::ConstFloat64(bits) => Some(
                otter_vm::Value::number(otter_vm::number::NumberValue::from_f64(f64::from_bits(
                    bits,
                )))
                .to_bits(),
            ),
            _ => None,
        };
        if let Some(bits) = constant {
            return self.constant_tagged(bits);
        }
        let kind = match node.repr {
            Repr::Int32 => Kind::Int32ToTagged,
            Repr::Float64 => Kind::Float64ToTagged,
            Repr::Tagged | Repr::None | Repr::Word => unreachable!("not a boxable value"),
        };
        let boxed = self.graph.add_node(kind, &[value], Repr::Tagged);
        self.graph.node_mut(boxed).block = Some(block);
        self.graph.block_mut(block).body.push(boxed);
        known.entry(value).tagged = Some(boxed);
        boxed
    }

    /// Leave an inlined body with `value` as the call's result. A construct
    /// returns its receiver for a value that is not an Object; a value not
    /// proved either way needs the activation's own completion.
    fn inline_return(&mut self, value: NodeId) {
        let Some(block) = self.current else {
            return;
        };
        let receiver = self.inline.as_ref().and_then(|inline| inline.receiver);
        let value = match receiver.map(|receiver| (receiver, self.is_object(value))) {
            None | Some((_, Some(true))) => value,
            Some((receiver, Some(false))) => receiver,
            Some((_, None)) => {
                self.inline.as_mut().expect("an inlined body").abandoned = true;
                return self.deopt(DeoptReason::Unsupported);
            }
        };
        let known = self.known.clone();
        let inline = self.inline.as_mut().expect("an inlined body");
        inline.returns.push((block, value, known));
        let continuation = inline.continuation;
        self.terminate(Kind::Jump(continuation), &[]);
    }

    // ------------------------------------------------------------------
    // Blocks, edges and merges
    // ------------------------------------------------------------------

    fn start_entry(&mut self, entry: BlockId) {
        self.current = Some(entry);
        let params = self.view.code_block.param_count.min(self.register_count);
        self.frame = vec![self.undefined; usize::from(self.register_count)];
        for register in 0..self.register_count {
            self.frame[usize::from(register)] = if register < params {
                self.add(Kind::InitialRegister(register), &[], Repr::Tagged)
            } else {
                self.undefined
            };
        }
        self.current_bytecode_block = usize::MAX;
        self.current_slot = usize::MAX;
        self.goto(0);
    }

    /// The OSR entry block: every register live at the loop header comes
    /// from the interpreter's window, and the block feeds the header as one
    /// more forward predecessor.
    fn start_osr_entry(&mut self, block: BlockId, header: usize) {
        self.current = Some(block);
        self.current_bytecode_block = usize::MAX;
        self.current_slot = usize::MAX;
        self.known = Known::default();
        let live = self
            .analysis
            .live_at(self.analysis.blocks[header].start)
            .clone();
        self.frame = vec![self.undefined; usize::from(self.register_count)];
        for register in 0..self.register_count {
            self.frame[usize::from(register)] = if live.contains(register) {
                self.add(Kind::InitialRegister(register), &[], Repr::Tagged)
            } else {
                self.undefined
            };
        }
        self.goto(header);
    }

    /// Begin bytecode block `block` in `slot`; `false` when nothing
    /// reaches it. The peeled copy of a loop header is no loop.
    fn start_block(&mut self, block: usize, slot: usize) -> bool {
        let mut incoming = std::mem::take(&mut self.incoming[slot]);
        if incoming.is_empty() {
            return false;
        }
        let graph_block = self.block_map[slot];
        self.current = Some(graph_block);
        self.current_bytecode_block = block;
        self.current_slot = slot;
        let is_loop = self.analysis.loops.contains_key(&block) && slot == block;
        let start_pc = self.analysis.blocks[block].start;
        let live = self.analysis.live_at(start_pc).clone();
        self.graph.block_mut(graph_block).predecessors =
            incoming.iter().map(|edge| edge.block).collect();
        self.graph.block_mut(graph_block).is_loop = is_loop;

        // The OSR entry lies in the compiled function's own body.
        let encloses_osr = is_loop
            && self.inline.is_none()
            && self.osr_header.is_some_and(|header| {
                header != block && self.analysis.loops[&block].body.contains(&header)
            });
        let mut frame = incoming[0].frame.clone();
        let mut phis = Vec::new();
        let mut field_phis = Vec::new();
        for register in 0..self.register_count {
            let index = usize::from(register);
            if !live.contains(register) {
                frame[index] = self.undefined;
                continue;
            }
            let first = incoming[0].frame[index];
            // An OSR entry inside a loop reaches its header along a back
            // edge only: there every live register is a merge, not just the
            // ones the body assigns. The OSR loop's own header merges the
            // entry block as a forward edge, and any other loop sees the
            // entry's values through its pre-header.
            let assigned = is_loop
                && (encloses_osr || self.analysis.loops[&block].assigned.contains(register));
            let differs = incoming.iter().any(|edge| edge.frame[index] != first);
            if !assigned && !differs {
                continue;
            }
            let inputs: SmallVec<[NodeId; 4]> = incoming
                .iter()
                .map(|edge| self.tagged_for_phi(edge, index))
                .collect();
            let phi = self.graph.add_node(Kind::Phi, &inputs, Repr::Tagged);
            self.graph.node_mut(phi).block = Some(graph_block);
            self.graph.block_mut(graph_block).phis.push(phi);
            frame[index] = phi;
            if !is_loop
                && let Some(bound) = field_type::merged_bound(
                    &self.graph,
                    &self.root.stable_shapes,
                    incoming.iter().map(|edge| (edge.frame[index], &edge.known)),
                )
            {
                field_phis.push((phi, bound));
            }
            if is_loop {
                phis.push((register, phi));
            }
        }
        if is_loop {
            self.loop_phis.insert(block, phis);
        }
        // Phis read each edge's own facts above; the merge takes the first
        // edge's and keeps what every other agrees on. A loop that encloses
        // the OSR entry is also entered along its back edge from that entry,
        // where no value of the forward path exists: its header keeps no
        // fact, as the OSR entry block itself starts with none.
        let mut known = if encloses_osr {
            Known::default()
        } else {
            std::mem::take(&mut incoming[0].known)
        };
        if !encloses_osr {
            for edge in &incoming[1..] {
                known.intersect(&edge.known);
            }
        }
        if is_loop {
            known.collect();
            known.checked_field_types.clear();
            known.stable_checked.clear();
            // The body reaches only the values its registers and `this`
            // hold: facts about any other node can only fail on a back edge.
            known.retain_reachable(frame.iter().copied().chain(known.this));
            let key = self.loop_key(block);
            match self.policies.get(&key) {
                Some(policy) if policy.clobber => known.clobber_heap(&self.root.stable_shapes),
                Some(policy) => known.forget(&policy.dropped),
                None => {}
            }
            self.assumptions.insert(key, known.heap_facts());
        }
        self.frame = frame;
        self.known = known;
        self.known.field_values.extend(field_phis);
        true
    }

    /// The tagged form of an incoming edge's register value. A phi merges
    /// tagged values only; an unboxed value is boxed at the end of its
    /// predecessor, which is where the edge's state was captured.
    fn tagged_for_phi(&mut self, edge: &Incoming, index: usize) -> NodeId {
        let value = edge.frame[index];
        if self.graph.node(value).repr == Repr::Tagged || self.graph.node(value).kind.is_constant()
        {
            return self.tagged(value);
        }
        edge.known
            .get(value)
            .and_then(|info| info.tagged)
            .expect("an edge carries a tagged form of every unboxed live value")
    }

    /// Box every live unboxed register so the outgoing state can feed phis.
    fn box_live_registers(&mut self, live: &RegisterSet) {
        for register in live.iter() {
            let value = self.frame[usize::from(register)];
            if self.graph.node(value).repr != Repr::Tagged {
                self.tagged(value);
            }
        }
    }

    /// Leave the current block along a normal edge to bytecode block
    /// `target`.
    fn goto(&mut self, target: usize) {
        let Some(current) = self.current else {
            return;
        };
        let from = self.current_bytecode_block;
        let back_edge = from != usize::MAX
            && self.analysis.loops.contains_key(&target)
            && self.analysis.blocks[target].start <= self.analysis.blocks[from].start
            && self.loop_phis.contains_key(&target);
        // A peeled iteration's latch enters the loop it precedes forward.
        let enters_loop = self.peel.as_ref().is_some_and(|peel| peel.header == target);
        let retreating = from != usize::MAX
            && self.analysis.blocks[target].start <= self.analysis.blocks[from].start;
        if retreating && !back_edge && !enters_loop {
            // An edge into a block already built that is not a natural
            // loop's back edge: resume the interpreter at the jump.
            self.deopt(DeoptReason::Unsupported);
            return;
        }
        let target_start = self.analysis.blocks[target].start;
        let live = self.analysis.live_at(target_start).clone();
        self.box_live_registers(&live);
        let slot = self.edge_slot(target, back_edge);
        let target_block = self.block_map[slot];
        if back_edge {
            let key = self.loop_key(target);
            if let Some(assumed) = self.assumptions.get(&key) {
                let broken: Vec<FactKey> = self.known.broken(assumed).collect();
                if !broken.is_empty() {
                    self.failed.entry(key).or_default().extend(broken);
                }
            }
            let phis = self.loop_phis[&target].clone();
            for (register, phi) in phis {
                let value = self.tagged(self.frame[usize::from(register)]);
                self.graph.node_mut(phi).inputs.push(value);
            }
            self.graph
                .block_mut(target_block)
                .predecessors
                .push(current);
            // An interrupt at the back edge resumes the interpreter at the
            // header with the values entering the next iteration.
            let header_pc = self.analysis.blocks[target].start;
            let state = self.frame_state(header_pc, &live, None);
            let jump = self.terminate(Kind::JumpLoop(target_block), &[]);
            self.graph.node_mut(jump).eager = Some(state);
            return;
        }
        // The block ends here: its state moves onto the edge, and the next
        // block or the inline caller installs its own.
        self.incoming[slot].push(Incoming {
            block: current,
            frame: std::mem::take(&mut self.frame),
            known: std::mem::take(&mut self.known),
        });
        self.terminate(Kind::Jump(target_block), &[]);
    }

    /// End the current block with a two-way branch whose targets are the
    /// bytecode blocks `if_true` and `if_false`.
    fn branch(&mut self, kind: BranchKind, inputs: &[NodeId], if_true: usize, if_false: usize) {
        let Some(current) = self.current else {
            return;
        };
        let true_block = self.edge_block(if_true);
        let false_block = self.edge_block(if_false);
        self.terminate(
            Kind::Branch {
                kind,
                if_true: true_block,
                if_false: false_block,
            },
            inputs,
        );
        // Each edge continues from its own split block with the branch's
        // facts: a box one edge materializes does not exist on the other.
        let mut state = Some((
            std::mem::take(&mut self.frame),
            std::mem::take(&mut self.known),
        ));
        for (edge, (split, target)) in [(true_block, if_true), (false_block, if_false)]
            .into_iter()
            .enumerate()
        {
            let (frame, known) = if edge == 1 {
                state
                    .take()
                    .expect("the second edge takes the branch state")
            } else {
                state
                    .clone()
                    .expect("the first edge copies the branch state")
            };
            self.frame = frame;
            self.known = known;
            self.current = Some(split);
            self.graph.block_mut(split).predecessors.push(current);
            self.refine_field_branch(kind, inputs, edge == 0);
            self.goto(target);
        }
        self.current = None;
    }

    /// Whether the conditional jump `op` over the constant `value` jumps, or
    /// `None` when the value is not a constant whose test needs no heap: a
    /// cell's truthiness (an empty string) is left to the branch.
    fn constant_condition(&self, op: Op, value: NodeId) -> Option<bool> {
        let Kind::ConstTagged(bits) = self.graph.node(value).kind else {
            return None;
        };
        let constant = otter_vm::Value::from_bits(bits);
        if op == Op::JumpIfNullish {
            return Some(constant.is_nullish());
        }
        let truthy = if constant.is_nullish() {
            false
        } else if let Some(boolean) = constant.as_boolean() {
            boolean
        } else {
            let number = constant.as_f64()?;
            number != 0.0 && !number.is_nan()
        };
        match op {
            Op::JumpIfTrue => Some(truthy),
            Op::JumpIfFalse => Some(!truthy),
            _ => None,
        }
    }

    /// A fresh block for one branch edge; it jumps on to `target`.
    fn edge_block(&mut self, _target: usize) -> BlockId {
        let block = self.graph.new_block();
        let owner = self.current_slot;
        if owner == usize::MAX {
            unreachable!("the entry block never branches");
        }
        self.trailing[owner].push(block);
        block
    }

    fn terminate(&mut self, kind: Kind, inputs: &[NodeId]) -> NodeId {
        let block = self.current.take().expect("an open block");
        let node = self.graph.add_node(kind, inputs, Repr::None);
        self.graph.node_mut(node).block = Some(block);
        self.graph.block_mut(block).control = Some(node);
        node
    }

    /// End the current block with an unconditional eager deopt at `pc`.
    fn deopt(&mut self, reason: DeoptReason) {
        if self.current.is_none() {
            return;
        }
        let state = self.eager_state();
        let block = self.current.take().expect("an open block");
        let node = self.graph.add_node(Kind::Deopt(reason), &[], Repr::None);
        let node_ref = self.graph.node_mut(node);
        node_ref.block = Some(block);
        node_ref.eager = Some(state);
        self.graph.block_mut(block).control = Some(node);
    }

    /// Clear the loop mark of headers whose back edges never arrived.
    fn finish_loops(&mut self) {
        for (&header, phis) in &self.loop_phis {
            let block = self.block_map[header];
            let predecessors = self.graph.block(block).predecessors.len();
            for &(_, phi) in phis {
                debug_assert_eq!(self.graph.node(phi).inputs.len(), predecessors);
            }
            let forward = predecessors
                - self
                    .graph
                    .block(block)
                    .predecessors
                    .iter()
                    .filter(|&&predecessor| {
                        self.graph
                            .block(predecessor)
                            .control
                            .is_some_and(|control| {
                                matches!(self.graph.node(control).kind, Kind::JumpLoop(_))
                            })
                    })
                    .count();
            if forward == predecessors {
                self.graph.block_mut(block).is_loop = false;
            }
        }
    }

    /// The finished loop headers: those whose back edges arrived, with the
    /// phis that survived trivial-phi removal.
    fn loop_headers(&self) -> Vec<LoopHeader> {
        let mut headers = Vec::new();
        for (&header, phis) in &self.loop_phis {
            let block = self.block_map[header];
            let data = self.graph.block(block);
            if !data.is_loop {
                continue;
            }
            let Some(state) = data.predecessors.iter().find_map(|&predecessor| {
                let control = self.graph.block(predecessor).control?;
                let node = self.graph.node(control);
                matches!(node.kind, Kind::JumpLoop(_))
                    .then_some(node.eager)
                    .flatten()
            }) else {
                continue;
            };
            let phis = phis
                .iter()
                .copied()
                .filter(|(_, phi)| data.phis.contains(phi))
                .collect();
            let header_pc = self.analysis.blocks[header].start;
            let exits = self.view.optimized_exit_reasons.get(&header_pc);
            let speculate =
                !exits.is_some_and(|reasons| reasons.contains(&ExitReason::TypeMismatch));
            let hoist = !exits.is_some_and(|reasons| reasons.contains(&ExitReason::ShapeGuard));
            let exit_reasons = exits
                .map(|reasons| reasons.iter().copied().collect())
                .unwrap_or_default();
            headers.push(LoopHeader {
                block,
                state,
                phis,
                speculate,
                hoist,
                exit_reasons,
            });
        }
        headers.sort_by_key(|header| header.block);
        headers
    }

    // ------------------------------------------------------------------
    // Nodes, frame states and representations
    // ------------------------------------------------------------------

    fn add(&mut self, kind: Kind, inputs: &[NodeId], repr: Repr) -> NodeId {
        let block = self.current.expect("an open block");
        let properties = kind.properties();
        let node = self.graph.add_node(kind, inputs, repr);
        self.graph.node_mut(node).block = Some(block);
        self.graph.block_mut(block).body.push(node);
        if properties.eager_deopt {
            let state = self.eager_state();
            self.graph.node_mut(node).eager = Some(state);
        }
        // Only a call, or a slow path that runs JavaScript, can change an
        // object's shape or prototype state; a slot store keeps every shape.
        // A collection only moves cells: it forgets raw storage words and
        // which cells are still young, never a shape or a field value.
        if properties.call || properties.runs_js {
            self.clobber_heap();
        } else if properties.may_collect {
            self.known.collect();
        }
        node
    }

    /// Forget heap facts after a call, which also makes every loop around
    /// the current block one that writes the heap.
    fn clobber_heap(&mut self) {
        let root = self.root;
        self.known.clobber_heap(&root.stable_shapes);
        if let Some(loops) = self.enclosing.get(self.current_bytecode_block) {
            self.effectful.extend(loops.iter().copied());
        }
    }

    /// The key of the loop headed by bytecode block `header` of the body
    /// being visited.
    fn loop_key(&self, header: usize) -> LoopKey {
        LoopKey {
            origin: self.graph.origin,
            header,
        }
    }

    /// Whether `SetPrototype` sets `null` on an object this body just
    /// allocated empty that nothing has read yet: the literal's own object,
    /// before its first property.
    fn null_prototype_literal(&self, instruction: &Instruction) -> bool {
        let [target, prototype] = [instruction.reads[0], instruction.reads[1]]
            .map(|register| self.frame[usize::from(register)]);
        self.view.literal_allocations.null_object.is_some()
            && self.graph.node(target).kind == Kind::NewObject
            && self.graph.node(prototype).kind
                == Kind::ConstTagged(otter_vm::Value::null().to_bits())
            && !self
                .graph
                .nodes
                .iter()
                .any(|node| node.inputs.contains(&target))
    }

    /// The values a rest list built from known values still holds: no node
    /// added since it was built writes the heap.
    pub(super) fn pristine_list_values(&self, list: NodeId) -> Option<SmallVec<[NodeId; 8]>> {
        let &(_, built) = self.pristine_lists.iter().find(|(node, _)| *node == list)?;
        let untouched = self.graph.nodes[built..]
            .iter()
            .all(|node| !node.kind.properties().writes);
        untouched.then(|| self.graph.node(list).inputs.iter().copied().collect())
    }

    fn constant_tagged(&mut self, bits: u64) -> NodeId {
        self.graph.constant(Kind::ConstTagged(bits))
    }

    fn constant_int32(&mut self, value: i32) -> NodeId {
        self.graph.constant(Kind::ConstInt32(value))
    }

    fn constant_float64(&mut self, value: f64) -> NodeId {
        self.graph.constant(Kind::ConstFloat64(value.to_bits()))
    }

    /// The canonical constant for a number: int32 when integral and in range
    /// (and not -0), double otherwise.
    fn constant_number(&mut self, value: f64) -> NodeId {
        let truncated = value as i32;
        if f64::from(truncated) == value && !(value == 0.0 && value.is_sign_negative()) {
            self.constant_int32(truncated)
        } else {
            self.constant_float64(value)
        }
    }

    /// The registers live before the current instruction, bound to their
    /// current values.
    fn eager_state(&mut self) -> FrameStateId {
        if let Some(&state) = self.eager_states.get(&self.pc) {
            return state;
        }
        let live = self.analysis.live_at(self.pc).clone();
        let state = self.frame_state(self.pc, &live, None);
        self.eager_states.insert(self.pc, state);
        state
    }

    /// The registers live after the current instruction, with `result`
    /// binding the instruction's written register to the node producing it.
    fn lazy_state(&mut self, result: Option<(u16, NodeId)>) -> FrameStateId {
        let next = self.pc + 1;
        let live = if (next as usize) < self.analysis.instructions.len() {
            self.analysis.live_at(next).clone()
        } else {
            RegisterSet::new(usize::from(self.register_count))
        };
        self.frame_state(next, &live, result)
    }

    fn frame_state(
        &mut self,
        pc: u32,
        live: &RegisterSet,
        result: Option<(u16, NodeId)>,
    ) -> FrameStateId {
        let registers = live
            .iter()
            .map(|register| match result {
                Some((written, node)) if written == register => (register, node),
                _ => (register, self.frame[usize::from(register)]),
            })
            .collect();
        let byte_pc = self
            .view
            .instructions
            .get(pc as usize)
            .map_or(self.view.code_block.bytecode_byte_len(), |metadata| {
                metadata.byte_pc
            });
        self.graph.add_frame_state(FrameState {
            pc,
            byte_pc,
            function_id: self.function_id,
            register_count: self.register_count,
            registers,
            caller: self.inline.as_ref().map(|inline| inline.caller.clone()),
        })
    }

    /// Current value of register `register`.
    fn read(&mut self, register: u16) -> NodeId {
        self.frame[usize::from(register)]
    }

    /// Bind register `register` to `value`.
    fn write(&mut self, register: u16, value: NodeId) {
        self.frame[usize::from(register)] = value;
    }

    /// The tagged form of `value`.
    fn tagged(&mut self, value: NodeId) -> NodeId {
        let node = self.graph.node(value);
        match (&node.kind, node.repr) {
            (_, Repr::Tagged) => return value,
            (Kind::ConstInt32(int), _) => {
                let int = *int;
                return self.constant_tagged(tag::NUMBER_TAG | u64::from(int as u32));
            }
            (Kind::ConstFloat64(bits), _) => {
                let number = otter_vm::number::NumberValue::from_f64(f64::from_bits(*bits));
                return self.constant_tagged(otter_vm::Value::number(number).to_bits());
            }
            _ => {}
        }
        if let Some(tagged) = self.known.get(value).and_then(|info| info.tagged) {
            return tagged;
        }
        let repr = self.graph.node(value).repr;
        let boxed = match repr {
            Repr::Int32 => self.add(Kind::Int32ToTagged, &[value], Repr::Tagged),
            Repr::Float64 => self.add(Kind::Float64ToTagged, &[value], Repr::Tagged),
            Repr::Tagged | Repr::None | Repr::Word => unreachable!("not a boxable value"),
        };
        self.known.entry(value).tagged = Some(boxed);
        let info = self.known.entry(boxed);
        match repr {
            Repr::Int32 => info.int32 = Some(value),
            _ => info.float64 = Some(value),
        }
        boxed
    }

    /// The int32 form of `value`, guarded by an eager deopt when unproved.
    fn int32(&mut self, value: NodeId) -> NodeId {
        let node = self.graph.node(value);
        match (&node.kind, node.repr) {
            (_, Repr::Int32) => return value,
            (Kind::ConstTagged(bits), _) => {
                if let Some(int) = otter_vm::Value::from_bits(*bits)
                    .as_number()
                    .and_then(|number| number.as_exact_i32())
                {
                    return self.constant_int32(int);
                }
            }
            _ => {}
        }
        if let Some(int) = self.known.get(value).and_then(|info| info.int32) {
            return int;
        }
        let repr = self.graph.node(value).repr;
        let unboxed = match repr {
            Repr::Tagged => self.add(Kind::CheckedNumberToInt32, &[value], Repr::Int32),
            Repr::Float64 => self.add(Kind::CheckedFloat64ToInt32, &[value], Repr::Int32),
            Repr::Int32 | Repr::None | Repr::Word => unreachable!("not an int32 source"),
        };
        self.known.entry(value).int32 = Some(unboxed);
        if repr == Repr::Tagged {
            self.known.entry(unboxed).tagged = Some(value);
        }
        unboxed
    }

    /// The float64 form of `value`, guarded by an eager deopt when unproved.
    fn float64(&mut self, value: NodeId) -> NodeId {
        let node = self.graph.node(value);
        match (&node.kind, node.repr) {
            (_, Repr::Float64) => return value,
            (Kind::ConstInt32(int), _) => {
                let int = *int;
                return self.constant_float64(f64::from(int));
            }
            (Kind::ConstTagged(bits), _) if bits & tag::NUMBER_TAG != 0 => {
                let value = otter_vm::Value::from_bits(*bits);
                if let Some(number) = value.as_number() {
                    return self.constant_float64(number.as_f64());
                }
            }
            _ => {}
        }
        if let Some(double) = self.known.get(value).and_then(|info| info.float64) {
            return double;
        }
        let repr = self.graph.node(value).repr;
        let unboxed = match repr {
            Repr::Tagged => self.add(Kind::CheckedTaggedToFloat64, &[value], Repr::Float64),
            Repr::Int32 => self.add(Kind::Int32ToFloat64, &[value], Repr::Float64),
            Repr::Float64 | Repr::None | Repr::Word => unreachable!("not a number source"),
        };
        self.known.entry(value).float64 = Some(unboxed);
        unboxed
    }

    // ------------------------------------------------------------------
    // Instructions
    // ------------------------------------------------------------------

    fn visit(&mut self, instruction: &Instruction) {
        let op = instruction.op;
        match op {
            Op::Nop => {}
            Op::LoadUndefined => self.set_constant(instruction, UNDEFINED),
            Op::LoadNull => self.set_constant(instruction, tag::OTHER_TAG),
            Op::LoadTrue => self.set_constant(instruction, tag::OTHER_TAG | tag::BOOL_TAG | 1),
            Op::LoadFalse => self.set_constant(instruction, tag::OTHER_TAG | tag::BOOL_TAG),
            Op::LoadInt32 => {
                let value = self.constant_int32(instruction.imm32(1).unwrap_or(0));
                self.write(instruction.writes[0], value);
            }
            Op::LoadNumber => {
                let number = self.view.instructions[self.pc as usize]
                    .load_number
                    .unwrap_or(f64::NAN);
                let value = self.constant_number(number);
                self.write(instruction.writes[0], value);
            }
            Op::LoadGlobalOrThrow | Op::LoadGlobalOrUndefined
                if !self.exited_for(ExitReason::ShapeGuard)
                    && self
                        .view
                        .binding_hit_proofs
                        .contains_key(&instruction.byte_pc) =>
            {
                let value = self.add(
                    Kind::LoadGlobalBinding(instruction.byte_pc),
                    &[],
                    Repr::Tagged,
                );
                self.write(instruction.writes[0], value);
            }
            Op::LoadGlobalThis
                if self.view.cage_base != 0 && !self.exited_for(ExitReason::IdentityGuard) =>
            {
                let value = self.add(Kind::LoadGlobalThis, &[], Repr::Tagged);
                self.prove_ordinary_object(value);
                self.write(instruction.writes[0], value);
            }
            Op::LoadLocal | Op::StoreLocal => {
                let value = self.read(instruction.reads[0]);
                self.write(instruction.writes[0], value);
            }
            Op::LoadSelf => {
                let value = self.closure();
                self.write(instruction.writes[0], value);
            }
            Op::NewObject => {
                let value = self.add(Kind::NewObject, &[], Repr::Tagged);
                self.prove_ordinary_object(value);
                self.write(instruction.writes[0], value);
            }
            // `{ __proto__: null, ... }`: the literal's fresh, still empty
            // object takes the null-prototype root shape at allocation.
            Op::SetPrototype if self.null_prototype_literal(instruction) => {
                let value = self.add(Kind::NewNullPrototypeObject, &[], Repr::Tagged);
                self.prove_ordinary_object(value);
                self.write(instruction.reads[0], value);
            }
            // A rest parameter of the physical activation: a spliced body's
            // actuals are its caller's values, which the baseline operation
            // reads.
            Op::CollectRest if self.inline.is_none() => {
                let formals = self.view.code_block.param_count;
                let value = self.add(Kind::NewRestArray(formals), &[], Repr::Tagged);
                self.prove_array_body(value);
                self.write(instruction.writes[0], value);
            }
            // A spliced body's rest list holds its call's trailing values.
            Op::CollectRest => {
                let formals = usize::from(self.view.code_block.param_count);
                let Some(arguments) = self
                    .inline
                    .as_ref()
                    .and_then(|inline| inline.caller.arguments.clone())
                else {
                    return self.generic(instruction);
                };
                let values: SmallVec<[NodeId; 8]> = arguments
                    .iter()
                    .skip(formals)
                    .map(|&value| self.tagged(value))
                    .collect();
                let value = self.add(Kind::NewArrayLiteral, &values, Repr::Tagged);
                self.prove_array_body(value);
                self.pristine_lists.push((value, self.graph.nodes.len()));
                self.write(instruction.writes[0], value);
            }
            Op::NewArray if instruction.const_index(1) == Some(0) => {
                let value = self.add(Kind::NewArrayEmpty, &[], Repr::Tagged);
                self.prove_array_body(value);
                self.write(instruction.writes[0], value);
            }
            Op::NewObjectLiteral | Op::NewArray | Op::NewArrayBoilerplate => {
                let inputs = instruction
                    .reads
                    .iter()
                    .map(|register| self.read(*register))
                    .collect::<Vec<_>>();
                let inputs = inputs
                    .into_iter()
                    .map(|value| self.tagged(value))
                    .collect::<Vec<_>>();
                let kind = if op == Op::NewObjectLiteral {
                    Kind::NewObjectLiteral {
                        fields: self.literal_field_owners(&inputs),
                    }
                } else {
                    Kind::NewArrayLiteral
                };
                let tenured = self
                    .view
                    .literal_allocations
                    .sites
                    .get(&self.pc)
                    .is_some_and(|site| site.mode == otter_vm::jit::JitSiteMode::Old);
                let object = matches!(kind, Kind::NewObjectLiteral { .. });
                let value = self.add(kind, &inputs, Repr::Tagged);
                if object {
                    // Its values seed the field types of its shape's owners.
                    self.known.checked_field_types.clear();
                    self.prove_ordinary_object(value);
                } else {
                    self.prove_array_body(value);
                }
                if tenured && object {
                    // A tenured object is born old: each young input it holds
                    // is an edge the next scavenge finds only if remembered.
                    for &input in &inputs {
                        self.write_barrier(value, input);
                    }
                }
                self.write(instruction.writes[0], value);
            }
            Op::CreateContext => {
                let Some(scope) = instruction
                    .imm32(2)
                    .and_then(|scope| u32::try_from(scope).ok())
                else {
                    return self.generic(instruction);
                };
                let parent = self.read(instruction.reads[0]);
                let parent = self.tagged(parent);
                let value = self.add(Kind::NativeNewContext(scope), &[parent], Repr::Tagged);
                self.write(instruction.writes[0], value);
            }
            Op::CopyContext => {
                let source = self.read(instruction.reads[0]);
                let source = self.tagged(source);
                let value = self.add(Kind::CopyContext, &[source], Repr::Tagged);
                self.write(instruction.writes[0], value);
            }
            Op::MakeFunction | Op::MakeClosure => {
                let inputs = if op == Op::MakeClosure {
                    let context = self.read(instruction.reads[0]);
                    let context = self.tagged(context);
                    let (this, new_target) = match &self.inline {
                        Some(inline) => (inline.caller.this, inline.caller.new_target),
                        None => {
                            let this = self.add(Kind::LoadThis, &[], Repr::Tagged);
                            let new_target = self.add(Kind::LoadNewTarget, &[], Repr::Tagged);
                            (this, new_target)
                        }
                    };
                    [context, this, new_target]
                } else {
                    [self.undefined; 3]
                };
                let value = self.add(Kind::NewClosure, &inputs, Repr::Tagged);
                self.write(instruction.writes[0], value);
            }
            Op::LoadNewTarget => {
                let value = match &self.inline {
                    Some(inline) => inline.caller.new_target,
                    None => self.add(Kind::LoadNewTarget, &[], Repr::Tagged),
                };
                self.write(instruction.writes[0], value);
            }
            Op::LoadClosureContext => {
                let closure = self.closure();
                // A proven method slot holds one heap closure, never a bare
                // function id: its context is one field away, and a body
                // baked against that closure reads its constants through it.
                let proven = self.inline.as_ref().is_some_and(|frame| frame.heap_closure);
                let unique = self.view.unique_closure_context;
                let value = if let Some(cell) = unique.filter(|_| !proven) {
                    self.add(
                        Kind::CheckClosureContext(self.view.code_block.id),
                        &[],
                        Repr::None,
                    );
                    self.add(Kind::LoadCellValue(cell), &[], Repr::Tagged)
                } else if proven {
                    let byte = self.view.closure_call_layout.context_byte as i32;
                    self.add(Kind::LoadTaggedField(byte), &[closure], Repr::Tagged)
                } else {
                    self.add(Kind::LoadClosureContext, &[closure], Repr::Tagged)
                };
                if (proven || unique.is_some()) && !self.view.closure_constants.is_empty() {
                    self.closure_contexts
                        .insert(value, (std::ptr::from_ref(self.view), 0));
                }
                self.write(instruction.writes[0], value);
            }
            Op::LoadThis if !self.view.derived_constructor => {
                let value = match &self.inline {
                    Some(inline) => inline.caller.this,
                    None if let Some(value) = self.known.this => value,
                    None => {
                        let value = self.add(Kind::LoadThis, &[], Repr::Tagged);
                        self.known.this = Some(value);
                        // A real ordinary sloppy entry already performed
                        // OrdinaryCallBindThis. Use the CodeBlock's canonical
                        // immutable entry contract; strict/lexical bindings
                        // may still be primitives, and inline bindings must
                        // retain only their caller's actual SSA proof.
                        if self.view.code_block.call_flags()
                            & otter_vm::native_abi::FUNCTION_CALL_NO_RECEIVER_CONVERSION
                            == 0
                        {
                            self.known.entry(value).heap_object = true;
                        }
                        // The receiver the body's own construct entry
                        // allocates has its family's root shape: proving
                        // it once lets every store to `this` follow the
                        // shape's transitions. An entry with any other
                        // receiver leaves once, and the body is rebuilt
                        // without the proof.
                        if let Some(plan) = self.view.own_receiver_plan
                            && !self.exited_for(ExitReason::ShapeGuard)
                        {
                            let root = plan.prototype_root;
                            self.check_shapes(value, &smallvec::smallvec![root], true);
                        }
                        value
                    }
                };
                self.write(instruction.writes[0], value);
            }
            Op::Jump => {
                let Flow::Jump { target } = instruction.flow else {
                    unreachable!("a jump has a target");
                };
                self.goto(self.analysis.block_of[target as usize]);
            }
            Op::JumpIfTrue | Op::JumpIfFalse | Op::JumpIfNullish => {
                self.visit_branch(instruction);
            }
            Op::Return | Op::ReturnValue => {
                let value = self.read(instruction.reads[0]);
                if self.inline.is_some() {
                    return self.inline_return(value);
                }
                let value = self.tagged(value);
                self.terminate(Kind::Return, &[value]);
            }
            Op::ReturnUndefined => {
                let value = self.undefined;
                if self.inline.is_some() {
                    return self.inline_return(value);
                }
                self.terminate(Kind::Return, &[value]);
            }
            Op::Add
            | Op::Sub
            | Op::Mul
            | Op::Div
            | Op::Rem
            | Op::BitwiseAnd
            | Op::BitwiseOr
            | Op::BitwiseXor
            | Op::Shl
            | Op::Shr
            | Op::Ushr => self.visit_binary(instruction, None),
            Op::AddImm | Op::SubImm | Op::BitwiseAndImm => {
                let immediate = instruction.imm32(2).unwrap_or(0);
                self.visit_binary(instruction, Some(immediate));
            }
            Op::Increment => {
                let delta = instruction.imm32(2).unwrap_or(1);
                self.visit_increment(instruction, delta);
            }
            Op::Neg | Op::BitwiseNot => self.visit_unary(instruction),
            Op::ToNumeric | Op::ToNumber => self.visit_to_numeric(instruction),
            Op::LessThan
            | Op::LessEq
            | Op::GreaterThan
            | Op::GreaterEq
            | Op::Equal
            | Op::NotEqual
            | Op::LooseEqual
            | Op::LooseNotEqual => self.visit_compare(instruction, None),
            Op::LessThanImm | Op::EqualImm | Op::NotEqualImm => {
                let immediate = instruction.imm32(2).unwrap_or(0);
                self.visit_compare(instruction, Some(immediate));
            }
            Op::LoadProperty => self.visit_load_property(instruction),
            Op::HasNamedProperty => self.visit_has_named_property(instruction),
            Op::StoreProperty | Op::StorePropertyStrict => self.visit_store_property(instruction),
            Op::Call => self.visit_call(instruction, false),
            Op::TailCall => self.visit_tail_call(instruction),
            Op::New => self.visit_call(instruction, true),
            Op::CallWithThis => self.visit_call_with_this(instruction),
            Op::CallMethodValue => self.visit_call_method(instruction),
            Op::CallForwardArguments => self.visit_call_forward(instruction),
            Op::LogicalNot | Op::ToBoolean => {
                let value = self.read(instruction.reads[0]);
                let value = self.tagged(value);
                let kind = if instruction.op == Op::LogicalNot {
                    Kind::LogicalNot
                } else {
                    Kind::ToBoolean
                };
                let result =
                    if kind == Kind::ToBoolean && self.graph.node(value).kind.produces_boolean() {
                        value
                    } else {
                        self.add(kind, &[value], Repr::Tagged)
                    };
                self.write(instruction.writes[0], result);
            }
            Op::LoadRegExp
                if self
                    .view
                    .literal_allocations
                    .regexps
                    .contains_key(&instruction.byte_pc) =>
            {
                let value = self.add(Kind::NewRegExp(instruction.byte_pc), &[], Repr::Tagged);
                self.write(instruction.writes[0], value);
            }
            Op::LoadString | Op::LoadBigInt
                if self.view.literal_cells.contains_key(&instruction.byte_pc) =>
            {
                let value = self.add(Kind::LoadLiteral(instruction.byte_pc), &[], Repr::Tagged);
                self.write(instruction.writes[0], value);
            }
            Op::TestTypeOf => {
                let Some(test) = instruction.imm32(2) else {
                    self.generic(instruction);
                    return;
                };
                let value = self.read(instruction.reads[0]);
                let value = self.tagged(value);
                let result = self.add(Kind::TestTypeOf { test }, &[value], Repr::Tagged);
                self.write(instruction.writes[0], result);
            }
            Op::Instanceof => {
                let value = self.read(instruction.reads[0]);
                let value = self.tagged(value);
                let target = self.read(instruction.reads[1]);
                let target = self.tagged(target);
                let result = self.add(Kind::Instanceof, &[value, target], Repr::Tagged);
                self.write(instruction.writes[0], result);
            }
            Op::LoadElement => self.visit_load_element(instruction),
            Op::LoadArgumentsLength => self.visit_arguments_length(instruction),
            Op::LoadArgumentsElement => self.visit_arguments_element(instruction),
            Op::StoreElement | Op::StoreElementStrict => self.visit_store_element(instruction),
            Op::LoadContextSlot => self.visit_load_context_slot(instruction, false),
            Op::LoadContextSlotChecked if !self.exited_for(ExitReason::IdentityGuard) => {
                self.visit_load_context_slot(instruction, true);
            }
            Op::StoreContextSlot => self.visit_store_context_slot(instruction),
            _ => self.generic(instruction),
        }
    }

    /// The body baked for the plain call at `byte_pc` when it calls
    /// `function_id` and no earlier code left the site on a wrong callee.
    fn inline_body(
        &self,
        byte_pc: u32,
        function_id: u32,
        callee: NodeId,
    ) -> Option<&'a Arc<JitCompileSnapshot>> {
        let view: &'a JitCompileSnapshot = self.view;
        // A constant closure keeps its identity whatever else left here.
        if self.exited_for(ExitReason::IdentityGuard)
            && self.constant_closures.get(&callee) != Some(&function_id)
        {
            return None;
        }
        view.inline_callees
            .get(&byte_pc)
            .filter(|callee| callee.function_id() == function_id)
            .map(|callee| &callee.body)
    }

    /// Whether the target observes the actual receiver unchanged. An Object
    /// needs no OrdinaryCallBindThis conversion even in a sloppy body. Arrows
    /// that observe `this` still require their closure's lexical binding.
    fn binds_this_as_is(
        &self,
        body: &JitCompileSnapshot,
        plan: otter_vm::jit::JitDirectCallPlan,
        receiver: NodeId,
    ) -> bool {
        let code = body.code_block.as_ref();
        !code.observes_this()
            || (!code.is_arrow()
                && (plan.this_mode == otter_vm::jit::JitDirectCallThisMode::StrictOrLexical
                    || (plan.this_mode == otter_vm::jit::JitDirectCallThisMode::SloppyGlobal
                        && self
                            .known
                            .get(receiver)
                            .is_some_and(|info| info.heap_object))))
    }

    /// The callable the body being visited runs as.
    fn closure(&mut self) -> NodeId {
        match &self.inline {
            Some(inline) => inline.caller.closure,
            None => self.add(Kind::LoadClosure, &[], Repr::Tagged),
        }
    }

    fn set_constant(&mut self, instruction: &Instruction, bits: u64) {
        let value = self.constant_tagged(bits);
        self.write(instruction.writes[0], value);
    }

    /// The instruction through its baseline operation on the frame window,
    /// or an unconditional deopt when it has none.
    fn generic(&mut self, instruction: &Instruction) {
        if let Some(original) = self.tail_call.take()
            && original.pc == instruction.pc
        {
            return self.generic(&original);
        }
        // A baseline operation runs on its own function's window, which an
        // inlined body does not have.
        if let Some(inline) = self.inline.as_mut() {
            inline.abandoned = true;
            self.deopt(DeoptReason::Unsupported);
            return;
        }
        if !self.baseline.has_operation(self.pc) {
            self.deopt(DeoptReason::Unsupported);
            return;
        }
        let mut inputs: SmallVec<[NodeId; 4]> = SmallVec::new();
        let mut registers = Vec::new();
        for &register in &instruction.reads {
            if registers.contains(&register) {
                continue;
            }
            let value = self.frame[usize::from(register)];
            inputs.push(self.tagged(value));
            registers.push(register);
        }
        let node = self.add(
            Kind::Generic {
                pc: self.pc,
                registers: registers.into_boxed_slice(),
            },
            &inputs,
            Repr::None,
        );
        let lazy = self.lazy_state(None);
        self.graph.node_mut(node).lazy = Some(lazy);
        self.clobber_heap();
        if instruction.flow == Flow::Exit {
            // The operation completes the frame; nothing follows it.
            self.deopt(DeoptReason::Unsupported);
            return;
        }
        for &register in &instruction.writes {
            let value = self.add(Kind::LoadWindow(register), &[], Repr::Tagged);
            self.frame[usize::from(register)] = value;
        }
    }

    fn feedback(&self) -> otter_vm::feedback::ArithFeedback {
        self.view.instructions[self.pc as usize].arith_feedback()
    }

    fn exited_before(&self) -> bool {
        self.view.optimized_exit_reasons.contains_key(&self.pc)
    }

    /// Whether optimized code left at this instruction for `reason`.
    fn exited_for(&self, reason: ExitReason) -> bool {
        self.view
            .optimized_exit_reasons
            .get(&self.pc)
            .is_some_and(|reasons| reasons.contains(&reason))
    }

    fn visit_binary(&mut self, instruction: &Instruction, immediate: Option<i32>) {
        let feedback = self.feedback();
        let op = instruction.op;
        let lhs = self.read(instruction.reads[0]);
        let rhs = match immediate {
            Some(value) => self.constant_int32(value),
            None => self.read(instruction.reads[1]),
        };
        let bitwise = matches!(
            op,
            Op::BitwiseAnd
                | Op::BitwiseOr
                | Op::BitwiseXor
                | Op::Shl
                | Op::Shr
                | Op::Ushr
                | Op::BitwiseAndImm
        );
        let int32 = feedback.speculates_int32() && !self.exited_before();
        let numeric = feedback.is_numeric_only() || int32;
        // A comparison's boolean converts to 0 or 1 without a check.
        let operands_convert = [lhs, rhs]
            .iter()
            .all(|&value| self.graph.node(value).kind.produces_boolean() || self.is_number(value));
        // `flags & x !== 0` (a precedence slip common in real code): the
        // boolean converts without a check and the other operand is proved a
        // Number. A site that left on that proof keeps the generic path.
        let boolean_operand = !self.exited_before()
            && [lhs, rhs]
                .iter()
                .any(|&value| self.graph.node(value).kind.produces_boolean());
        if bitwise && (numeric || operands_convert || boolean_operand) {
            let a = self.truncated_int32(lhs, int32);
            let b = self.truncated_int32(rhs, int32);
            // `>>>` is unsigned: once a result left the int32 range here, it
            // is computed as a double instead of checked again.
            if op == Op::Ushr && !int32 {
                let result = self.add(Kind::Uint32ShiftRightToFloat64, &[a, b], Repr::Float64);
                self.write(instruction.writes[0], result);
                return;
            }
            // `ToInt32` already happened on the operands: `x | 0`, `x ^ 0`,
            // `x << 0`, `x >> 0` and `x & -1` are the int32 operand itself.
            let identity = match (op, self.int32_constant(a), self.int32_constant(b)) {
                (Op::BitwiseOr | Op::BitwiseXor, _, Some(0))
                | (Op::Shl | Op::Shr, _, Some(0))
                | (Op::BitwiseAnd | Op::BitwiseAndImm, _, Some(-1)) => Some(a),
                (Op::BitwiseOr | Op::BitwiseXor, Some(0), _)
                | (Op::BitwiseAnd | Op::BitwiseAndImm, Some(-1), _) => Some(b),
                _ => None,
            };
            if let Some(value) = identity {
                self.write(instruction.writes[0], value);
                return;
            }
            let kind = match op {
                Op::BitwiseAnd | Op::BitwiseAndImm => Kind::Int32BitAnd,
                Op::BitwiseOr => Kind::Int32BitOr,
                Op::BitwiseXor => Kind::Int32BitXor,
                Op::Shl => Kind::Int32ShiftLeft,
                Op::Shr => Kind::Int32ShiftRight,
                _ => Kind::Int32ShiftRightLogical,
            };
            let result = self.add(kind, &[a, b], Repr::Int32);
            self.write(instruction.writes[0], result);
            return;
        }
        // `count += a !== b`: a comparison's boolean is the Number 0 or 1,
        // and the other operand is proved a Number.
        if boolean_operand
            && !bitwise
            && !self.exited_for(ExitReason::TypeMismatch)
            && matches!(
                op,
                Op::Add | Op::AddImm | Op::Sub | Op::SubImm | Op::Mul | Op::Div | Op::Rem
            )
        {
            let [a, b] = [lhs, rhs].map(|value| {
                if self.graph.node(value).kind.produces_boolean() {
                    let int = self.add(Kind::BooleanToInt32, &[value], Repr::Int32);
                    self.add(Kind::Int32ToFloat64, &[int], Repr::Float64)
                } else {
                    self.float64(value)
                }
            });
            let kind = match op {
                Op::Add | Op::AddImm => Kind::Float64Add,
                Op::Sub | Op::SubImm => Kind::Float64Sub,
                Op::Mul => Kind::Float64Mul,
                Op::Div => Kind::Float64Div,
                _ => Kind::Float64Mod,
            };
            let result = self.add(kind, &[a, b], Repr::Float64);
            self.write(instruction.writes[0], result);
            return;
        }
        if int32 {
            let a = self.int32(lhs);
            let b = self.int32(rhs);
            let kind = match op {
                Op::Add | Op::AddImm => Kind::Int32Add,
                Op::Sub | Op::SubImm => Kind::Int32Sub,
                Op::Mul => Kind::Int32Mul,
                Op::Div => Kind::Int32Div,
                _ => Kind::Int32Mod,
            };
            let result = self.add(kind, &[a, b], Repr::Int32);
            self.write(instruction.writes[0], result);
            return;
        }
        if numeric {
            let a = self.float64(lhs);
            let b = self.float64(rhs);
            let kind = match op {
                Op::Add | Op::AddImm => Kind::Float64Add,
                Op::Sub | Op::SubImm => Kind::Float64Sub,
                Op::Mul => Kind::Float64Mul,
                Op::Div => Kind::Float64Div,
                _ => Kind::Float64Mod,
            };
            let result = self.add(kind, &[a, b], Repr::Float64);
            self.write(instruction.writes[0], result);
            return;
        }
        if op == Op::Add && feedback.is_primitive_string_concat_only() {
            // A side proved a Number joins as its cached decimal string, so
            // the concatenation itself sees two strings.
            let numbers = [self.is_number(lhs), self.is_number(rhs)];
            let mut operands = [lhs, rhs];
            for (operand, number) in operands.iter_mut().zip(numbers) {
                let tagged = self.tagged(*operand);
                *operand = if number && numbers != [true, true] {
                    self.add(Kind::NumberToString, &[tagged], Repr::Tagged)
                } else {
                    tagged
                };
            }
            let [a, b] = operands;
            let result = self.add(Kind::PrimitiveAdd, &[a, b], Repr::Tagged);
            self.write(instruction.writes[0], result);
            return;
        }
        if feedback.is_bigint_only()
            && let Some(operator) = bigint_operator(op)
        {
            let a = self.tagged(lhs);
            let b = self.tagged(rhs);
            let result = self.add(Kind::BigIntBinary(operator), &[a, b], Repr::Tagged);
            self.write(instruction.writes[0], result);
            return;
        }
        self.generic(instruction);
    }

    /// The value of an int32 constant node.
    fn int32_constant(&self, node: NodeId) -> Option<i32> {
        match self.graph.node(node).kind {
            Kind::ConstInt32(value) => Some(value),
            _ => None,
        }
    }

    /// An int32 operand of a bitwise operator: `ToInt32` of the number or
    /// the comparison's boolean. An unboxed double truncates, which never
    /// fails; a tagged value is checked to be an exact int32 Number when
    /// the site only saw such numbers.
    fn truncated_int32(&mut self, value: NodeId, int32: bool) -> NodeId {
        if self.graph.node(value).kind.produces_boolean() {
            return self.add(Kind::BooleanToInt32, &[value], Repr::Int32);
        }
        let repr = self.graph.node(value).repr;
        let known_int32 = self.known.get(value).and_then(|info| info.int32);
        if repr == Repr::Int32 || known_int32.is_some() || (int32 && repr == Repr::Tagged) {
            return self.int32(value);
        }
        let double = self.float64(value);
        self.add(Kind::TruncateFloat64ToInt32, &[double], Repr::Int32)
    }

    fn visit_increment(&mut self, instruction: &Instruction, delta: i32) {
        let feedback = self.feedback();
        let value = self.read(instruction.reads[0]);
        if feedback.speculates_int32() && !self.exited_before() {
            let a = self.int32(value);
            let b = self.constant_int32(delta);
            let result = self.add(Kind::Int32Add, &[a, b], Repr::Int32);
            self.write(instruction.writes[0], result);
        } else if feedback.is_numeric_only() {
            let a = self.float64(value);
            let b = self.constant_float64(f64::from(delta));
            let result = self.add(Kind::Float64Add, &[a, b], Repr::Float64);
            self.write(instruction.writes[0], result);
        } else {
            self.generic(instruction);
        }
    }

    /// Whether `value` is always `true` or `false`: a boolean constant, a
    /// node that produces one, or a merge of such values. A loop header's
    /// phi may still gain inputs, so it proves nothing.
    fn is_boolean(&self, value: NodeId) -> bool {
        let mut pending: SmallVec<[NodeId; 8]> = SmallVec::new();
        let mut seen: SmallVec<[NodeId; 8]> = SmallVec::new();
        pending.push(value);
        while let Some(value) = pending.pop() {
            if seen.contains(&value) {
                continue;
            }
            if seen.len() == seen.inline_size() {
                return false;
            }
            seen.push(value);
            let node = self.graph.node(value);
            match node.kind {
                Kind::ConstTagged(bits) if bits == tag::VALUE_TRUE || bits == tag::VALUE_FALSE => {}
                Kind::Phi => {
                    let Some(block) = node.block else {
                        return false;
                    };
                    if self.graph.block(block).is_loop {
                        return false;
                    }
                    pending.extend(node.inputs.iter().copied());
                }
                ref kind if kind.produces_boolean() => {}
                _ => return false,
            }
        }
        true
    }

    /// Whether `value` is proved to be a Number on this path.
    fn is_number(&self, value: NodeId) -> bool {
        let node = self.graph.node(value);
        match (&node.kind, node.repr) {
            (_, Repr::Int32 | Repr::Float64) => return true,
            (Kind::ConstTagged(bits), _) => return tag::is_number_bits(*bits),
            _ => {}
        }
        self.known
            .get(value)
            .is_some_and(|info| info.number || info.int32.is_some() || info.float64.is_some())
    }

    /// `ToNumeric` / `ToNumber` of a Number is the value itself; anything
    /// else leaves through the check, or runs generically once it has.
    fn visit_to_numeric(&mut self, instruction: &Instruction) {
        let value = self.read(instruction.reads[0]);
        if self.is_number(value) {
            self.write(instruction.writes[0], value);
            return;
        }
        if self.exited_before() {
            self.generic(instruction);
            return;
        }
        self.add(Kind::CheckNumber, &[value], Repr::None);
        self.known.entry(value).number = true;
        self.write(instruction.writes[0], value);
    }

    fn visit_unary(&mut self, instruction: &Instruction) {
        let feedback = self.feedback();
        let value = self.read(instruction.reads[0]);
        let int32 = feedback.speculates_int32() && !self.exited_before();
        match instruction.op {
            Op::Neg if int32 => {
                let a = self.int32(value);
                let result = self.add(Kind::Int32Negate, &[a], Repr::Int32);
                self.write(instruction.writes[0], result);
            }
            Op::Neg if feedback.is_numeric_only() => {
                let a = self.float64(value);
                let result = self.add(Kind::Float64Negate, &[a], Repr::Float64);
                self.write(instruction.writes[0], result);
            }
            Op::BitwiseNot if int32 || feedback.is_numeric_only() => {
                let a = self.truncated_int32(value, int32);
                let result = self.add(Kind::Int32BitNot, &[a], Repr::Int32);
                self.write(instruction.writes[0], result);
            }
            _ => self.generic(instruction),
        }
    }

    fn condition_of(op: Op) -> Option<Condition> {
        Some(match op {
            Op::LessThan | Op::LessThanImm => Condition::Less,
            Op::LessEq => Condition::LessEqual,
            Op::GreaterThan => Condition::Greater,
            Op::GreaterEq => Condition::GreaterEqual,
            Op::Equal | Op::EqualImm | Op::LooseEqual => Condition::Equal,
            Op::NotEqual | Op::NotEqualImm | Op::LooseNotEqual => Condition::NotEqual,
            _ => return None,
        })
    }

    fn visit_compare(&mut self, instruction: &Instruction, immediate: Option<i32>) {
        let Some(condition) = Self::condition_of(instruction.op) else {
            self.generic(instruction);
            return;
        };
        let feedback = self.feedback();
        let lhs = self.read(instruction.reads[0]);
        let rhs = match immediate {
            Some(value) => self.constant_int32(value),
            None => self.read(instruction.reads[1]),
        };
        if matches!(instruction.op, Op::LooseEqual | Op::LooseNotEqual) {
            let nullish = |value| {
                matches!(self.graph.node(value).kind, Kind::ConstTagged(bits)
                    if bits == tag::VALUE_NULL || bits == tag::VALUE_UNDEFINED)
            };
            let other = if nullish(lhs) {
                Some(rhs)
            } else if nullish(rhs) {
                Some(lhs)
            } else {
                None
            };
            if let Some(other) = other {
                let other = self.tagged(other);
                let value = self.field_nullish_equal(other, instruction.op == Op::LooseNotEqual);
                self.write(instruction.writes[0], value);
                return;
            }
            // Two booleans compare loosely as they compare strictly: no
            // operand converts, and nothing can run JavaScript.
            if self.is_boolean(lhs) && self.is_boolean(rhs) {
                self.visit_strict_equal(instruction, lhs, rhs, condition);
                return;
            }
        }
        let strict = matches!(
            instruction.op,
            Op::Equal | Op::NotEqual | Op::EqualImm | Op::NotEqualImm
        );
        // An explicit non-Number constant rules out numeric speculation,
        // including at sites whose feedback is still empty.
        if strict
            && [lhs, rhs].iter().any(|&value| {
                matches!(self.graph.node(value).kind, Kind::ConstTagged(bits)
                    if !tag::is_number_bits(bits))
            })
        {
            self.visit_strict_equal(instruction, lhs, rhs, condition);
            return;
        }
        // Operands already held as int32 compare as int32 whatever the site
        // saw elsewhere: no conversion, no speculation.
        let held_int32 = [lhs, rhs]
            .iter()
            .all(|&value| self.graph.node(value).repr == Repr::Int32);
        let int32 = held_int32 || (feedback.speculates_int32() && !self.exited_before());
        // Loose equality of two Numbers is their strict equality.
        let proved_numbers = self.is_number(lhs) && self.is_number(rhs);
        let numeric = feedback.is_numeric_only() || int32 || proved_numbers;
        if !numeric {
            if strict {
                self.visit_strict_equal(instruction, lhs, rhs, condition);
            } else if feedback.is_primitive_string_concat_only() {
                let a = self.tagged(lhs);
                let b = self.tagged(rhs);
                let value = self.add(Kind::PrimitiveCompare(condition), &[a, b], Repr::Tagged);
                self.write(instruction.writes[0], value);
            } else if matches!(instruction.op, Op::LooseEqual | Op::LooseNotEqual)
                // A site that left on a converting pair keeps its baseline
                // operation, which completes every pair.
                && !self.exited_for(ExitReason::TypeMismatch)
            {
                let a = self.tagged(lhs);
                let b = self.tagged(rhs);
                let negate = instruction.op == Op::LooseNotEqual;
                let value = self.add(Kind::LooseEqual { negate }, &[a, b], Repr::Tagged);
                self.write(instruction.writes[0], value);
            } else {
                self.generic(instruction);
            }
            return;
        }
        let (branch, inputs) = if int32 {
            let a = self.int32(lhs);
            let b = self.int32(rhs);
            (BranchKind::Int32(condition), [a, b])
        } else {
            let a = self.float64(lhs);
            let b = self.float64(rhs);
            (BranchKind::Float64(condition), [a, b])
        };
        let destination = instruction.writes[0];
        // Fuse with an immediately following conditional jump on the result
        // when nothing else reads it.
        let next_pc = self.pc + 1;
        let analysis = Rc::clone(&self.analysis);
        if let Some(next) = analysis.instructions.get(next_pc as usize)
            && analysis.block_of[next_pc as usize] == self.current_bytecode_block
            && matches!(next.op, Op::JumpIfTrue | Op::JumpIfFalse)
            && next.reads.first() == Some(&destination)
            && let Flow::Branch { target } = next.flow
        {
            let after = next_pc + 1;
            let dead_after = (after as usize) >= self.analysis.instructions.len()
                || !self.analysis.live_at(after).contains(destination)
                    && !self.analysis.live_at(target).contains(destination);
            if dead_after {
                let value = match branch {
                    BranchKind::Int32(condition) => {
                        self.add(Kind::Int32Compare(condition), &inputs, Repr::Tagged)
                    }
                    _ => self.add(Kind::Float64Compare(condition), &inputs, Repr::Tagged),
                };
                // The comparison value stays defined for frame states of
                // the jump; the branch itself re-tests the operands.
                self.write(destination, value);
                self.pc = next_pc;
                let taken = self.analysis.block_of[target as usize];
                let fallthrough = self.analysis.block_of[after as usize];
                let (if_true, if_false) = if next.op == Op::JumpIfTrue {
                    (taken, fallthrough)
                } else {
                    (fallthrough, taken)
                };
                self.branch(branch, &inputs, if_true, if_false);
                return;
            }
        }
        let kind = match branch {
            BranchKind::Int32(condition) => Kind::Int32Compare(condition),
            _ => Kind::Float64Compare(condition),
        };
        let value = self.add(kind, &inputs, Repr::Tagged);
        self.write(destination, value);
    }

    /// `===` / `!==` on values that are not all Numbers. An operand that is
    /// an immediate other than a Number (`undefined`, `null`, a boolean) makes
    /// identity exact; otherwise the full strict comparison runs inline.
    fn visit_strict_equal(
        &mut self,
        instruction: &Instruction,
        lhs: NodeId,
        rhs: NodeId,
        condition: Condition,
    ) {
        let a = self.tagged(lhs);
        let b = self.tagged(rhs);
        let negate = condition == Condition::NotEqual;
        let oddball = |graph: &Graph, node: NodeId| match graph.node(node).kind {
            Kind::ConstTagged(bits) => !tag::is_number_bits(bits),
            _ => false,
        };
        let identity = oddball(&self.graph, a) || oddball(&self.graph, b);
        let destination = instruction.writes[0];
        let next_pc = self.pc + 1;
        let analysis = Rc::clone(&self.analysis);
        if identity
            && let Some(next) = analysis.instructions.get(next_pc as usize)
            && analysis.block_of[next_pc as usize] == self.current_bytecode_block
            && matches!(next.op, Op::JumpIfTrue | Op::JumpIfFalse)
            && next.reads.first() == Some(&destination)
            && let Flow::Branch { target } = next.flow
        {
            let after = next_pc + 1;
            let dead_after = (after as usize) >= self.analysis.instructions.len()
                || !self.analysis.live_at(after).contains(destination)
                    && !self.analysis.live_at(target).contains(destination);
            if dead_after {
                let value = self.add(Kind::StrictEqual { negate }, &[a, b], Repr::Tagged);
                self.write(destination, value);
                self.pc = next_pc;
                let taken = self.analysis.block_of[target as usize];
                let fallthrough = self.analysis.block_of[after as usize];
                // The branch tests identity: its true edge is `===`.
                let jumps_on_equal = (next.op == Op::JumpIfTrue) != negate;
                let (if_true, if_false) = if jumps_on_equal {
                    (taken, fallthrough)
                } else {
                    (fallthrough, taken)
                };
                self.branch(BranchKind::TaggedEqual, &[a, b], if_true, if_false);
                return;
            }
        }
        let value = self.add(Kind::StrictEqual { negate }, &[a, b], Repr::Tagged);
        self.write(destination, value);
    }

    fn visit_branch(&mut self, instruction: &Instruction) {
        let Flow::Branch { target } = instruction.flow else {
            unreachable!("a conditional jump has a target");
        };
        let raw = self.read(instruction.reads[0]);
        let taken = self.analysis.block_of[target as usize];
        let fallthrough = self.analysis.block_of[self.pc as usize + 1];
        // An ordinary object is truthy, and a Number already unboxed tests
        // its value.
        if let Some((if_truthy, if_falsy)) = match instruction.op {
            Op::JumpIfTrue => Some((taken, fallthrough)),
            Op::JumpIfFalse => Some((fallthrough, taken)),
            _ => None,
        } && !self.graph.node(raw).kind.is_constant()
        {
            if self.is_ordinary_object(raw) {
                self.goto(if_truthy);
                return;
            }
            let info = self.known.get(raw).copied().unwrap_or_default();
            let int32 = (self.graph.node(raw).repr == Repr::Int32)
                .then_some(raw)
                .or(info.int32);
            let float64 = (self.graph.node(raw).repr == Repr::Float64)
                .then_some(raw)
                .or(info.float64);
            if let Some(value) = int32 {
                let zero = self.constant_int32(0);
                let kind = BranchKind::Int32(Condition::NotEqual);
                self.branch(kind, &[value, zero], if_truthy, if_falsy);
                return;
            }
            if let Some(value) = float64 {
                self.branch(BranchKind::Float64Truthy, &[value], if_truthy, if_falsy);
                return;
            }
        }
        let value = self.tagged(raw);
        // A condition known while building selects its edge now; the other
        // successor gets no incoming edge from here.
        if let Some(jumps) = self.constant_condition(instruction.op, value) {
            self.goto(if jumps { taken } else { fallthrough });
            return;
        }
        match instruction.op {
            Op::JumpIfTrue => self.branch(BranchKind::Truthy, &[value], taken, fallthrough),
            Op::JumpIfFalse => self.branch(BranchKind::Truthy, &[value], fallthrough, taken),
            _ => self.branch(BranchKind::Nullish, &[value], taken, fallthrough),
        }
    }

    // ------------------------------------------------------------------
    // Property and context access
    // ------------------------------------------------------------------

    fn visit_load_property(&mut self, instruction: &Instruction) {
        let byte_pc = instruction.byte_pc;
        let object = self.read(instruction.reads[0]);
        let object = self.tagged(object);
        let site = &self.view.instructions[self.pc as usize];
        let array_proved = self.known.get(object).is_some_and(|info| info.array_body);
        if site.load_array_length && array_proved && !self.exited_for(ExitReason::ShapeGuard) {
            let value = self.add(
                Kind::LoadArrayLength { array_proved: true },
                &[object],
                Repr::Int32,
            );
            return self.write(instruction.writes[0], value);
        }
        if self.try_accessor_call(instruction, None) || self.load_receiver_method(instruction) {
            return;
        }
        let receivers = (!self.exited_for(ExitReason::ShapeGuard))
            .then(|| self.known_shapes(object))
            .flatten();
        // A site that already left on a receiver its shapes did not describe
        // is compiled to the lookup that serves every receiver.
        let Some(access) = (!self.exited_for(ExitReason::ShapeGuard))
            .then(|| super::feedback::own_data_load(self.view, byte_pc, receivers.as_deref()))
            .flatten()
        else {
            return self.visit_named_load(instruction);
        };
        let object = self.property_storage(object, access.projection);
        self.check_shapes(object, &access.shapes, false);
        let key = FieldKey {
            object,
            property: true,
            offset: access.field.cache_key() as i32,
        };
        if let Some(&value) = self.known.fields.get(&key) {
            self.write(instruction.writes[0], value);
            return;
        }
        let int32 = self.field_load_int32(&access.shapes, access.field);
        if int32.is_none()
            && let Some(double_only) = self.field_load_number(&access.shapes, access.field)
        {
            let value = self.add(
                Kind::LoadNumberOwnField {
                    field: access.field,
                    atom: access.atom,
                    double_only,
                },
                &[object],
                Repr::Float64,
            );
            self.known.fields.insert(key, value);
            self.write(instruction.writes[0], value);
            return;
        }
        let bound = int32
            .is_none()
            .then(|| self.field_load_bound(&access.shapes, access.field))
            .flatten();
        let value = match int32 {
            Some(field_type::FieldInt32::Constant(value)) => self.constant_int32(value),
            Some(field_type::FieldInt32::Payload) => self.add(
                Kind::LoadOwnField {
                    field: access.field,
                    atom: access.atom,
                },
                &[object],
                Repr::Int32,
            ),
            None => self.add(
                Kind::LoadOwnField {
                    field: access.field,
                    atom: access.atom,
                },
                &[object],
                Repr::Tagged,
            ),
        };
        if let Some(bound) = bound {
            self.known.field_values.insert(value, bound);
        }
        self.known.fields.insert(key, value);
        self.write(instruction.writes[0], value);
    }

    /// `name in object` at a site whose receivers all owned the key: their
    /// shapes fix the answer, so it is `true` behind the shape check. Any
    /// other site keeps the baseline operation.
    fn visit_has_named_property(&mut self, instruction: &Instruction) {
        let Some(shapes) = (!self.exited_for(ExitReason::ShapeGuard))
            .then(|| super::feedback::own_key_shapes(self.view, instruction.byte_pc))
            .flatten()
        else {
            return self.generic(instruction);
        };
        let object = self.read(instruction.reads[0]);
        let object = self.tagged(object);
        self.check_shapes(object, &shapes, false);
        self.set_constant(instruction, tag::OTHER_TAG | tag::BOOL_TAG | 1);
    }

    /// A named load that is not one own data slot: a load through its
    /// feedback programs, a deopt for a site that never ran, or the
    /// baseline operation.
    fn visit_named_load(&mut self, instruction: &Instruction) {
        let byte_pc = instruction.byte_pc;
        let object = self.read(instruction.reads[0]);
        let object = self.tagged(object);
        let receivers = (!self.exited_for(ExitReason::ShapeGuard))
            .then(|| self.known_shapes(object))
            .flatten();
        let shared = self
            .view
            .property_accesses
            .get(&byte_pc)
            .is_some_and(|site| site.shared);
        let programs = (!self.exited_for(ExitReason::ShapeGuard) && !shared)
            .then(|| super::feedback::named_load_programs(self.view, byte_pc, receivers.as_deref()))
            .flatten()
            .is_some();
        if !programs {
            let atom = self
                .view
                .property_accesses
                .get(&byte_pc)
                .filter(|site| site.shared);
            let site = &self.view.instructions[self.pc as usize];
            // A site that has read only Arrays reads the length word.
            if site.load_array_length
                && site.length_reads_arrays_only
                && !self.exited_for(ExitReason::ShapeGuard)
            {
                let value = self.add(
                    Kind::LoadArrayLength {
                        array_proved: false,
                    },
                    &[object],
                    Repr::Int32,
                );
                self.prove_array_body(object);
                return self.write(instruction.writes[0], value);
            }
            // A site that has read only typed views reads the count word.
            if site.load_array_length
                && site.length_reads_typed_arrays_only
                && self.view.typed_array_length.type_tag != 0
                && !self.exited_for(ExitReason::ShapeGuard)
            {
                let value = self.add(Kind::LoadTypedArrayLength, &[object], Repr::Int32);
                self.known.entry(object).heap_object = true;
                return self.write(instruction.writes[0], value);
            }
            // A site that has read only primitive strings reads the length
            // word; a string's length never changes.
            if site.load_array_length
                && site.length_reads_strings_only
                && !self.exited_for(ExitReason::TypeMismatch)
            {
                let value = self.add(Kind::LoadStringLength, &[object], Repr::Int32);
                return self.write(instruction.writes[0], value);
            }
            if self.view.instructions[self.pc as usize].load_array_length {
                let value = self.add(
                    Kind::LoadPropertyCached {
                        pc: self.pc,
                        length: true,
                    },
                    &[object],
                    Repr::Tagged,
                );
                return self.write(instruction.writes[0], value);
            }
            if atom.is_none()
                && !self.view.property_programs.contains_key(&byte_pc)
                && !self.view.instructions[self.pc as usize].property_attempted
            {
                return self.deopt(DeoptReason::InsufficientFeedback);
            }
        }
        let value = self.named_lookup(byte_pc, object);
        self.write(instruction.writes[0], value);
    }

    /// `[[Get]]` of the named site at `byte_pc` on `object`: the site's
    /// feedback programs inline. Megamorphic sites first use the complete
    /// shape-owned data directory. A typed own-data miss permanently selects
    /// the committed shared lookup for that site; cache residency never deopts
    /// a directory read.
    fn named_lookup(&mut self, byte_pc: u32, object: NodeId) -> NodeId {
        let receivers = (!self.exited_for(ExitReason::ShapeGuard))
            .then(|| self.known_shapes(object))
            .flatten();
        // A megamorphic site's programs read intrinsic receivers only; the
        // shared table answers every ordinary one.
        let shared = self
            .view
            .property_accesses
            .get(&byte_pc)
            .is_some_and(|site| site.shared);
        if !self.exited_for(ExitReason::ShapeGuard)
            && !shared
            && let Some(programs) =
                super::feedback::named_load_programs(self.view, byte_pc, receivers.as_deref())
        {
            let receiver_shape = match (
                programs.programs.as_slice(),
                receivers.as_deref(),
                programs.receiver_shapes.as_deref(),
            ) {
                ([_], Some([known]), Some([selected])) if known == selected => Some(*known),
                _ => None,
            };
            let value = self.add(
                Kind::LoadNamedProperty {
                    byte_pc,
                    programs: programs.programs,
                    receiver_shape,
                },
                &[object],
                Repr::Tagged,
            );
            // Every program proved an ordinary receiver of one of its shapes.
            if let Some(shapes) = programs.receiver_shapes {
                let layout = self.known.layout_entry(object);
                if layout.shapes.is_none() {
                    layout.shapes = Some(shapes);
                    layout.writable = false;
                }
                self.prove_ordinary_object(object);
            }
            return value;
        }
        let atom = self
            .view
            .property_accesses
            .get(&byte_pc)
            .filter(|site| site.shared)
            .map(|site| site.atom);
        if let Some(atom) = atom.filter(|_| !self.exited_for(ExitReason::OwnDataMiss)) {
            return self.add(Kind::LoadShapedProperty { atom }, &[object], Repr::Tagged);
        }
        self.add(
            Kind::LoadPropertyCached {
                pc: self.pc,
                length: false,
            },
            &[object],
            Repr::Tagged,
        )
    }

    fn visit_store_property(&mut self, instruction: &Instruction) {
        if self.try_accessor_call(instruction, Some(instruction.reads[1])) {
            return;
        }
        self.visit_store_property_regs(instruction, instruction.reads[0], instruction.reads[1]);
    }

    /// The named store of the value in `value_reg` into the object in
    /// `object_reg`, by the programs of `instruction`'s site.
    fn visit_store_property_regs(
        &mut self,
        instruction: &Instruction,
        object_reg: u16,
        value_reg: u16,
    ) {
        let receiver = self.read(object_reg);
        let receivers = self.known_shapes(receiver);
        let Some(access) = (!self.exited_for(ExitReason::ShapeGuard))
            .then(|| {
                super::feedback::own_data_store(
                    self.view,
                    instruction.byte_pc,
                    receivers.as_deref(),
                )
            })
            .flatten()
        else {
            return self.visit_named_store(instruction, object_reg, value_reg);
        };
        let object = self.read(object_reg);
        let object = self.tagged(object);
        let value = self.read(value_reg);
        let double = self.graph.node(value).repr == Repr::Float64;
        let value = if double { value } else { self.tagged(value) };
        let object = self.property_storage(object, access.projection);
        self.check_shapes(object, &access.shapes, true);
        let Some(owners) = self.field_store_owners(&access.shapes, access.field, value) else {
            return self.visit_named_store(instruction, object_reg, value_reg);
        };
        self.add(
            if double {
                Kind::StoreDoubleOwnField {
                    field: access.field,
                    atom: access.atom,
                    owners,
                }
            } else {
                Kind::StoreOwnField {
                    field: access.field,
                    atom: access.atom,
                    owners,
                }
            },
            &[object, value],
            Repr::None,
        );
        if !double {
            self.write_barrier(object, value);
        }
        self.known.store_field(
            FieldKey {
                object,
                property: true,
                offset: access.field.cache_key() as i32,
            },
            value,
        );
        // Later Phi rewriting can choose an encoded Double for this Tagged
        // input. Final store selection may therefore turn its observer on.
        self.known.checked_field_types.clear();
        // A slot write keeps the object's shape.
        let layout = self.known.layout_entry(object);
        layout.shapes = Some(access.shapes.clone());
        layout.writable = true;
    }

    /// A named store of the value in `value_reg` into the object in
    /// `object_reg` that is not one existing own slot: a store through its
    /// feedback programs, a deopt for a site that never ran, or the baseline
    /// operation.
    fn visit_named_store(&mut self, instruction: &Instruction, object_reg: u16, value_reg: u16) {
        self.known.checked_field_types.clear();
        let byte_pc = instruction.byte_pc;
        // An exotic body's own data slot: the write keeps every ordinary
        // object's shape and fields.
        if !self.exited_for(ExitReason::ShapeGuard)
            && let Some(slot) = super::feedback::body_slot_store(self.view, byte_pc)
        {
            let object = self.read(object_reg);
            let object = self.tagged(object);
            let value = self.read(value_reg);
            let value = self.tagged(value);
            self.add(Kind::StoreBodySlot(slot), &[object, value], Repr::None);
            self.write_barrier(object, value);
            self.known.entry(object).heap_object = true;
            return;
        }
        // A megamorphic site's receiver proved to have one shape runs the
        // program a monomorphic site would hold for that shape.
        let proved = if self.exited_for(ExitReason::ShapeGuard)
            || super::feedback::named_store_programs(self.view, byte_pc).is_some()
        {
            None
        } else {
            let object = self.read(object_reg);
            let view: &'a JitCompileSnapshot = self.view;
            match self.known_shapes(object).as_deref() {
                Some(&[shape]) => view
                    .proved_store_programs
                    .get(&(byte_pc, shape))
                    .map(std::slice::from_ref),
                _ => None,
            }
        };
        let site = proved
            .map(|programs| (otter_vm::jit::PropertyStorageProjection::Receiver, programs))
            .or_else(|| {
                (!self.exited_for(ExitReason::ShapeGuard))
                    .then(|| super::feedback::named_store_programs(self.view, byte_pc))
                    .flatten()
            });
        if let Some((projection, programs)) = site {
            use otter_vm::JitCacheIrOp as Op;
            let proved = proved.is_some();
            let object = self.read(object_reg);
            let object = self.tagged(object);
            let object = self.property_storage(object, projection);
            let source_value = self.read(value_reg);
            let receiver_operand =
                if projection == otter_vm::jit::PropertyStorageProjection::ArrayExpando {
                    1
                } else {
                    0
                };
            let guarded = |program: &otter_vm::jit::JitCacheIrProgram| {
                program.ops.iter().find_map(|op| match *op {
                    Op::GuardShape {
                        object: operand,
                        shape,
                    } if operand == receiver_operand => Some(shape),
                    _ => None,
                })
            };
            // A receiver proved to have one shape runs only the programs
            // that guard it.
            let receiver_shape = match self.known_shapes(object).as_deref() {
                Some(&[shape])
                    if receiver_operand == 0
                        && programs
                            .iter()
                            .any(|program| guarded(program) == Some(shape)) =>
                {
                    Some(shape)
                }
                _ => None,
            };
            let compatible = |program: &otter_vm::JitCacheIrProgram| {
                receiver_shape.is_none_or(|shape| guarded(program) == Some(shape))
            };
            let mut selected = programs.iter().filter(|program| compatible(program));
            let source_shape = selected.next().and_then(guarded);
            let one_source =
                source_shape.is_some() && selected.all(|program| guarded(program) == source_shape);
            // A selected transition's immutable geometry belongs to its
            // source program. Its guards all miss before the original store.
            let candidate = one_source
                .then(|| {
                    programs.iter().enumerate().find(|(_, program)| {
                        compatible(program) && super::feedback::inline_store_transition(program)
                    })
                })
                .flatten();
            let mut plan = NamedStorePlan::Programs;
            if projection == otter_vm::jit::PropertyStorageProjection::Receiver
                && let Some((index, program)) = candidate
                && super::feedback::inline_store_transition(program)
            {
                let node = self.graph.node(source_value);
                let numeric = node.repr == Repr::Float64
                    || matches!(node.kind, Kind::ConstTagged(bits)
                        if otter_vm::Value::from_bits(bits).is_number() && !otter_vm::Value::from_bits(bits).is_int32())
                    || self
                        .known
                        .get(source_value)
                        .is_some_and(|known| known.float64.is_some() && known.int32.is_none());
                plan = if numeric {
                    NamedStorePlan::TransitionDouble(index as u8)
                } else {
                    NamedStorePlan::TransitionTagged(index as u8)
                };
            }
            // Pure numeric conversion retains the original source SSA value in
            // the eager StoreProperty frame; no speculative conversion is added.
            let value = if plan.is_double() {
                self.float64(source_value)
            } else {
                self.tagged(source_value)
            };
            let Some(owners) = self.named_store_owners(programs, projection, object_reg, value)
            else {
                return self.generic(instruction);
            };
            let fresh = self.known.fresh.contains(&object);
            self.add(
                Kind::StoreNamedProperty {
                    plan,
                    byte_pc,
                    owners,
                    receiver_shape,
                    fresh,
                    proved,
                },
                &[object, value],
                Repr::None,
            );
            if !plan.is_double() {
                self.write_barrier(object, value);
            }
            // The receiver now has the shape each matching program leaves it
            // with; a published transition changes the shape of an object of
            // its old shape, which any node proved to have that shape may name.
            let mut shapes: SmallVec<[u32; 4]> = SmallVec::new();
            let mut offsets: SmallVec<[i32; 4]> = SmallVec::new();
            let mut left: SmallVec<[u32; 4]> = SmallVec::new();
            for (index, program) in programs.iter().enumerate() {
                if plan
                    .program()
                    .is_some_and(|selected| usize::from(selected) != index)
                {
                    continue;
                }
                let from = guarded(program);
                if receiver_shape.is_some() && from != receiver_shape {
                    continue;
                }
                let mut shape = from;
                for op in program.ops.iter() {
                    match *op {
                        Op::PublishShape { shape: child, .. } => {
                            shape = Some(child);
                            match from {
                                Some(from) if !left.contains(&from) => left.push(from),
                                Some(_) => {}
                                // An unguarded transition may leave any shape.
                                None => left.push(u32::MAX),
                            }
                        }
                        Op::StoreField { field, .. } => {
                            offsets.push(field.cache_key() as i32);
                        }
                        _ => {}
                    }
                }
                if let Some(shape) = shape
                    && !shapes.contains(&shape)
                {
                    shapes.push(shape);
                }
            }
            if left.contains(&u32::MAX) {
                let root = self.root;
                self.known.forget_shapes(&root.stable_shapes);
            } else if !left.is_empty() {
                self.known.forget_shapes_from(&left);
            }
            for &offset in &offsets {
                self.known.forget_field_offset(true, offset);
            }
            if let [offset] = offsets.as_slice() {
                let key = FieldKey {
                    object,
                    property: true,
                    offset: *offset,
                };
                self.known.fields.insert(key, value);
            }
            let layout = self.known.layout_entry(object);
            layout.shapes = Some(shapes);
            layout.writable = true;
            self.known.entry(object).heap_object = true;
            return;
        }
        let atom = self
            .view
            .property_accesses
            .get(&byte_pc)
            .filter(|site| site.shared)
            .map(|site| site.atom);
        if atom.is_none()
            && !self.view.property_programs.contains_key(&byte_pc)
            && !self.view.instructions[self.pc as usize].property_attempted
        {
            return self.deopt(DeoptReason::InsufficientFeedback);
        }
        // A megamorphic site, or one whose receivers no program describes.
        // The runtime completion serves the sloppy-or-function-strict form
        // only.
        if instruction.op != Op::StoreProperty {
            return self.generic(instruction);
        }
        let object = self.read(object_reg);
        let object = self.tagged(object);
        let value = self.read(value_reg);
        let value = self.tagged(value);
        self.add(
            Kind::StorePropertyCached { pc: self.pc },
            &[object, value],
            Repr::None,
        );
    }

    /// One selected record per program of a named store site, in program
    /// order, judged on the stored value after that program's own effects.
    /// An append `S→T` leaves a receiver-aliased value with shape `T`: the
    /// same SSA value is exactly `{T}`, a bound holding `S` may alias it and
    /// gains `T`, and a bound without `S` cannot be the receiver. Distinct
    /// nodes prove no non-alias. `None` when a program has no field authority.
    fn named_store_owners(
        &mut self,
        programs: &[otter_vm::JitCacheIrProgram],
        projection: otter_vm::jit::PropertyStorageProjection,
        object_reg: u16,
        value: NodeId,
    ) -> Option<SmallVec<[FieldStoreOwner; 4]>> {
        use otter_vm::JitCacheIrOp as Op;
        let operand =
            u8::from(projection == otter_vm::jit::PropertyStorageProjection::ArrayExpando);
        let receiver = self.read(object_reg);
        let aliased = receiver == value;
        let bound = self.store_value_bound(value);
        programs
            .iter()
            .map(|program| {
                let guarded = program.ops.iter().find_map(|op| match *op {
                    Op::GuardShape { object, shape } if object == operand => Some(shape),
                    _ => None,
                })?;
                let published = program.ops.iter().find_map(|op| match *op {
                    Op::PublishShape { shape, .. } => Some(shape),
                    _ => None,
                });
                let field = program.ops.iter().find_map(|op| match *op {
                    Op::StoreField { field, .. } => Some(field),
                    _ => None,
                })?;
                let record = self
                    .view
                    .field_types
                    .get(&(published.unwrap_or(guarded), field))?;
                let after = match published {
                    None => bound.clone(),
                    Some(target) if aliased => Some(StoreValueBound::exact(target)),
                    Some(target) => bound.as_ref().and_then(|bound| {
                        if bound.shapes.contains(&guarded) {
                            bound.with_shape(target)
                        } else {
                            Some(bound.clone())
                        }
                    }),
                };
                Some(FieldStoreOwner::select(record, after.as_ref()))
            })
            .collect()
    }

    /// Materialize a tagged storage owner while source state retains its receiver.
    fn property_storage(
        &mut self,
        receiver: NodeId,
        projection: otter_vm::jit::PropertyStorageProjection,
    ) -> NodeId {
        match projection {
            otter_vm::jit::PropertyStorageProjection::Receiver => receiver,
            otter_vm::jit::PropertyStorageProjection::ArrayExpando => {
                let bag = self.add(
                    Kind::LoadArrayExpando(self.view.array_layout),
                    &[receiver],
                    Repr::Tagged,
                );
                self.known.entry(bag).heap_object = true;
                bag
            }
            otter_vm::jit::PropertyStorageProjection::LookupStart => {
                let bag = match self.constant_bags.get(&receiver) {
                    Some(&cell) => self.add(Kind::LoadFunctionBagCell(cell), &[], Repr::Tagged),
                    None => self.add(Kind::LoadFunctionBag, &[receiver], Repr::Tagged),
                };
                self.known.entry(bag).heap_object = true;
                bag
            }
            otter_vm::jit::PropertyStorageProjection::TypedArrayStart => {
                unreachable!("a typed array's start lowers only through its programs")
            }
        }
    }

    /// Prove `object` has one of `shapes`, unless already proved.
    /// The generational barrier after storing `value` into `object`, unless
    /// `value` can never be a cell: an immediate constant, a boxed number or
    /// a boolean result; or unless `object` is a cell this path just bumped.
    fn write_barrier(&mut self, object: NodeId, value: NodeId) {
        if self.owes_barrier(object, value) {
            self.add(Kind::WriteBarrier, &[object, value], Repr::None);
        }
    }

    /// Whether storing `value` into `object` may need a value barrier: a
    /// fresh cell is young and unmarked, and a never-cell value owes none.
    fn owes_barrier(&self, object: NodeId, value: NodeId) -> bool {
        if self.known.fresh.contains(&object) {
            return false;
        }
        !match self.graph.node(value).kind {
            Kind::ConstTagged(bits) => !tag::is_cell_bits(bits),
            Kind::Int32ToTagged | Kind::Float64ToTagged => true,
            ref kind => kind.produces_boolean(),
        }
    }

    /// An ordinary object birth or shape check proves an immutable body kind.
    fn prove_ordinary_object(&mut self, object: NodeId) {
        let info = self.known.entry(object);
        info.heap_object = true;
        info.ordinary_object = true;
    }

    /// Array body kind is immutable; this proves no current length or layout.
    fn prove_array_body(&mut self, array: NodeId) {
        let info = self.known.entry(array);
        info.heap_object = true;
        info.array_body = true;
    }

    /// `receiver` passed a method guard on its exact `shape`, which holds
    /// until a heap write may change it.
    fn prove_receiver_shape(&mut self, receiver: NodeId, shape: u32) {
        let layout = self.known.layout_entry(receiver);
        match layout.shapes.as_ref() {
            None => layout.writable = false,
            Some(shapes) if shapes.contains(&shape) => {}
            Some(_) => return self.prove_ordinary_object(receiver),
        }
        layout.shapes = Some(smallvec::smallvec![shape]);
        self.prove_ordinary_object(receiver);
    }

    /// The shapes `object` is proved to have on this path: a current proof,
    /// or a stale proof of stable shapes, re-proved here as a stability
    /// dependency (each once per overtaking operation).
    fn known_shapes(&mut self, object: NodeId) -> Option<SmallVec<[u32; 4]>> {
        if let Some(shapes) = self
            .known
            .layout(object)
            .and_then(|layout| layout.shapes.clone())
        {
            return Some(shapes);
        }
        let Some(shapes) = self.known.stale_shapes.get(&object).cloned() else {
            return self.field_result_shapes(object);
        };
        let unchecked: SmallVec<[u32; 4]> = shapes
            .iter()
            .copied()
            .filter(|shape| !self.known.stable_checked.contains(shape))
            .collect();
        if !unchecked.is_empty() {
            self.add(Kind::CheckStableShapes(unchecked.clone()), &[], Repr::None);
            self.known.stable_checked.extend(unchecked);
        }
        self.known.stale_shapes.remove(&object);
        let layout = self.known.layout_entry(object);
        layout.shapes = Some(shapes.clone());
        layout.writable = false;
        Some(shapes)
    }

    /// Prove `object` has one of `shapes`. A writable site's shapes come from
    /// store handlers, which no prototype shape receives, so a proof within
    /// them also excludes prototypes.
    fn check_shapes(&mut self, object: NodeId, shapes: &SmallVec<[u32; 4]>, writable: bool) {
        if let Some(known) = self.known_shapes(object)
            && known.iter().all(|shape| shapes.contains(shape))
        {
            if writable {
                self.known.layout_entry(object).writable = true;
            }
            return;
        }
        let cell_proved = self
            .known
            .get(object)
            .is_some_and(|info| info.heap_object || info.ordinary_object);
        self.add(
            Kind::CheckShapes {
                shapes: shapes.clone(),
                writable,
                cell_proved,
            },
            &[object],
            Repr::None,
        );
        let layout = self.known.layout_entry(object);
        layout.shapes = Some(shapes.clone());
        layout.writable = writable;
        self.prove_ordinary_object(object);
    }

    // ------------------------------------------------------------------
    // Calls
    // ------------------------------------------------------------------

    /// `dst = callee(args...)`, or `dst = new callee(args...)` when
    /// `construct`. A site that never ran leaves to collect its target; a
    /// site with one proven target enters it directly.
    /// §15.10.3 `return f(args…)` from a strict tail position, to a site's
    /// one bytecode target: an ordinary call and return, which the callee
    /// can then be inlined into, while the native stack keeps
    /// [`super::ir::TAIL_CALL_STACK_RESERVE`] free. Past it the guard resumes the
    /// baseline operation, which replaces the frame, so an unbounded chain
    /// of tail calls still runs in bounded stack. Any other site keeps the
    /// baseline operation.
    fn visit_tail_call(&mut self, instruction: &Instruction) {
        let monomorphic = self
            .view
            .direct_callees
            .get(&instruction.byte_pc)
            .is_some_and(|targets| targets.len() == 1);
        let Some(destination) = instruction.register(0) else {
            return self.generic(instruction);
        };
        if !monomorphic || self.exited_for(ExitReason::UnsupportedOperation) {
            return self.generic(instruction);
        }
        self.add(Kind::CheckStackHeadroom, &[], Repr::None);
        let mut call = instruction.clone();
        call.op = Op::Call;
        call.writes = smallvec::smallvec![destination];
        call.flow = Flow::Next;
        self.tail_call = Some(instruction.clone());
        self.visit_call(&call, false);
        self.tail_call = None;
        if self.current.is_none() {
            return;
        }
        let value = self.read(destination);
        if self.inline.is_some() {
            return self.inline_return(value);
        }
        let value = self.tagged(value);
        self.terminate(Kind::Return, &[value]);
    }

    fn visit_call(&mut self, instruction: &Instruction, construct: bool) {
        let (Some(callee), Some(argc)) = (instruction.register(1), instruction.const_index(2))
        else {
            return self.generic(instruction);
        };
        let Ok(argc) = usize::try_from(argc) else {
            return self.generic(instruction);
        };
        let arguments: Option<SmallVec<[u16; 8]>> = (0..argc)
            .map(|index| instruction.register(3 + index))
            .collect();
        let Some(arguments) = arguments else {
            return self.generic(instruction);
        };
        if !self.view.instructions[self.pc as usize].call_attempted {
            return self.deopt(DeoptReason::InsufficientFeedback);
        }
        if self.emit_intrinsic_call(instruction, callee, &arguments, construct) {
            return;
        }
        // A native leaf target runs on its operand words without a frame;
        // one this site may not speculate on keeps the baseline operation.
        if !construct
            && self
                .view
                .native_calls
                .get(&instruction.byte_pc)
                .is_some_and(|target| target.leaf().is_some())
        {
            let Some((target, declaration)) = self.admitted_native_leaf(instruction, argc) else {
                return self.generic(instruction);
            };
            let callee = self.read(callee);
            let callee = self.tagged(callee);
            let arguments: SmallVec<[NodeId; 8]> = arguments
                .iter()
                .map(|&register| {
                    let value = self.read(register);
                    self.tagged(value)
                })
                .collect();
            let undefined = self.undefined;
            return self.emit_native_leaf_call(
                instruction,
                target,
                declaration,
                callee,
                undefined,
                &arguments,
            );
        }
        // `[[Construct]]` enters a proven target directly only when it has
        // the internal method; classification throws otherwise.
        let construct_target = construct
            .then(|| self.view.direct_constructs.get(&instruction.byte_pc))
            .flatten()
            .filter(|target| {
                target.plan.call_flags & otter_vm::native_abi::FUNCTION_CALL_CONSTRUCTIBLE != 0
            });
        // A base constructor's receiver is allocated in generated code when
        // the target's allocation plan names it as new.target.
        let allocation = construct_target.and_then(|target| {
            target.receiver_allocation.filter(|allocation| {
                allocation.new_target_function_id == target.plan.function_id
                    && !target.plan.is_derived_constructor
            })
        });
        let plan = if construct {
            construct_target.map(|target| target.plan)
        } else {
            self.view
                .direct_callees
                .get(&instruction.byte_pc)
                .filter(|targets| targets.len() == 1)
                .map(|targets| targets[0].plan)
        };
        let callee = self.read(callee);
        let callee = self.tagged(callee);
        if !construct
            && let Some(plan) = plan
            && let Some(body) = self.inline_body(instruction.byte_pc, plan.function_id, callee)
        {
            let values: SmallVec<[NodeId; 8]> = arguments
                .iter()
                .map(|&register| self.read(register))
                .collect();
            let undefined = self.undefined;
            if self.binds_this_as_is(body, plan, undefined)
                && self.inline_call(instruction, body, plan, callee, undefined, &values)
            {
                return;
            }
        }
        // A closure or class constructing itself allocates its receiver in
        // place when generated code may allocate at all. A class runs its
        // constructor body with the class as new.target.
        if let Some(plan) = plan
            && let Some(allocation) = allocation
            && self.view.literal_allocations.group_allowed
            && let Some(body) = self.inline_body(instruction.byte_pc, plan.function_id, callee)
        {
            let values: SmallVec<[NodeId; 8]> = arguments
                .iter()
                .map(|&register| self.read(register))
                .collect();
            let checkpoint = self.current.map(|block| self.graph.checkpoint(block));
            let closure = if allocation.new_target_is_class {
                self.add(Kind::LoadClassConstructor, &[callee], Repr::Tagged)
            } else {
                callee
            };
            if self.inline_construct(
                instruction,
                body,
                plan,
                allocation,
                closure,
                callee,
                &values,
            ) {
                return;
            }
            if let Some(checkpoint) = checkpoint
                && closure != callee
            {
                self.graph.rollback(checkpoint);
            }
        }
        let mut inputs: SmallVec<[NodeId; 4]> = SmallVec::new();
        inputs.push(callee);
        for register in arguments {
            let value = self.read(register);
            inputs.push(self.tagged(value));
        }
        let node = self.add(
            Kind::CallJs {
                pc: self.pc,
                plan: crate::call_linkage::CallPlan::for_site(self.view, instruction.byte_pc, plan),
                construct,
                receiver: false,
                receiver_object: false,
                allocation,
            },
            &inputs,
            Repr::Tagged,
        );
        let destination = instruction.writes[0];
        let lazy = self.lazy_state(Some((destination, node)));
        self.graph.node_mut(node).lazy = Some(lazy);
        self.write(destination, node);
    }

    /// The argument registers of a call whose count is the constant operand
    /// at `count_operand`, followed by the registers themselves.
    fn call_arguments(
        instruction: &Instruction,
        count_operand: usize,
    ) -> Option<SmallVec<[u16; 8]>> {
        let argc = usize::try_from(instruction.const_index(count_operand)?).ok()?;
        (0..argc)
            .map(|index| instruction.register(count_operand + 1 + index))
            .collect()
    }

    /// A call with an explicit receiver, entering `plan`'s target directly
    /// when the callee is proved to be it.
    fn emit_call_with_receiver(
        &mut self,
        instruction: &Instruction,
        callee: NodeId,
        receiver: NodeId,
        arguments: &[NodeId],
        plan: Option<otter_vm::jit::JitDirectCallPlan>,
    ) {
        // Strict non-arrows bind this value as-is; sloppy bodies also do so
        // when this actual receiver already has an Object proof.
        if let Some(plan) = plan
            && let Some(body) = self.inline_body(instruction.byte_pc, plan.function_id, callee)
            && self.binds_this_as_is(body, plan, receiver)
            && self.inline_call(instruction, body, plan, callee, receiver, arguments)
        {
            return;
        }
        self.emit_receiver_call(
            instruction,
            callee,
            receiver,
            arguments,
            plan,
            Some(instruction.writes[0]),
        );
    }

    /// The out-of-line call of `callee` with `receiver` bound as `this`; its
    /// result goes to `destination`, or nowhere for an instruction that
    /// writes no register.
    fn emit_receiver_call(
        &mut self,
        instruction: &Instruction,
        callee: NodeId,
        receiver: NodeId,
        arguments: &[NodeId],
        plan: Option<otter_vm::jit::JitDirectCallPlan>,
        destination: Option<u16>,
    ) {
        // The realm's own RegExp `exec` / `test` or String `replace` /
        // `split` / `match` / `search`, its identity proved, runs through its
        // runtime entry without a native call frame; an argument the call
        // does not pass is the `undefined` the method would read.
        if !self.exited_for(ExitReason::IdentityGuard)
            && let Some(&otter_vm::JitNativeCall::Intrinsic(proof)) =
                self.view.native_calls.get(&instruction.byte_pc)
            && let Some(form) = crate::intrinsic_calls::method_form(
                proof,
                self.view.literal_allocations.realm_id,
                arguments.len(),
            )
        {
            self.add(
                Kind::CheckCellValue(proof.identity_cell),
                &[callee],
                Repr::None,
            );
            let mut inputs: SmallVec<[NodeId; 3]> = SmallVec::new();
            inputs.push(receiver);
            inputs.extend(arguments.iter().copied());
            while inputs.len() < 1 + form.arity() {
                inputs.push(self.graph.constant(Kind::ConstTagged(UNDEFINED)));
            }
            let node = self.add(Kind::BuiltinMethod(form), &inputs, Repr::Tagged);
            if let Some(destination) = destination {
                self.write(destination, node);
            }
            return;
        }
        let mut inputs: SmallVec<[NodeId; 4]> = SmallVec::new();
        inputs.push(callee);
        inputs.push(receiver);
        inputs.extend(arguments.iter().copied());
        // A sloppy callee converts a receiver that is not an Object; one
        // proved here enters past that test.
        let receiver_object = plan.is_some_and(|plan| {
            plan.this_mode == otter_vm::jit::JitDirectCallThisMode::SloppyGlobal
        }) && self.is_object(receiver) == Some(true);
        let node = self.add(
            Kind::CallJs {
                pc: self.pc,
                plan: crate::call_linkage::CallPlan::for_site(self.view, instruction.byte_pc, plan),
                construct: false,
                receiver: true,
                receiver_object,
                allocation: None,
            },
            &inputs,
            Repr::Tagged,
        );
        let lazy = self.lazy_state(destination.map(|destination| (destination, node)));
        self.graph.node_mut(node).lazy = Some(lazy);
        if let Some(destination) = destination {
            self.write(destination, node);
        }
    }

    /// A named load or store whose receivers reached one getter or setter
    /// through an inherited accessor: the method-target guard (receiver
    /// shape, prototype proof), the getter or setter read from the
    /// accessor's cell, and a direct call of it with the receiver bound as
    /// `this` — the stored value as its argument for a store. `false` when
    /// the site has no such target, its guard already failed here, or it saw
    /// several.
    fn try_accessor_call(&mut self, instruction: &Instruction, value: Option<u16>) -> bool {
        let byte_pc = instruction.byte_pc;
        if self.exited_for(ExitReason::ShapeGuard) || self.exited_for(ExitReason::IdentityGuard) {
            return false;
        }
        let view: &'a JitCompileSnapshot = self.view;
        let Some(methods) = view.method_targets.get(&byte_pc) else {
            return false;
        };
        let Some((target, method, target_callee)) = single_method_target(methods) else {
            // A getter several receiver classes reach: each class's arm
            // calls (or splices) the getter its guard proves.
            let accessors = methods.iter().filter(|method| method.callee.is_some());
            if value.is_some()
                || instruction.writes.is_empty()
                || accessors.clone().count() < 2
                || accessors
                    .clone()
                    .any(|method| method.guard.accessor_byte.is_none())
            {
                return false;
            }
            let receiver = self.read(instruction.reads[0]);
            let receiver = self.tagged(receiver);
            self.emit_polymorphic_method_call(instruction, receiver, &[], methods, false);
            return true;
        };
        if method.guard.accessor_byte.is_none() {
            return false;
        }
        let receiver = self.read(instruction.reads[0]);
        let receiver = self.tagged(receiver);
        let arguments: SmallVec<[NodeId; 1]> = value
            .map(|register| {
                let value = self.read(register);
                self.tagged(value)
            })
            .into_iter()
            .collect();
        let callee = self.guarded_method(byte_pc, target, false, method, receiver);
        self.prove_receiver_shape(receiver, method.guard.recv_shape);
        let destination = instruction.writes.first().copied();
        self.emit_receiver_call(
            instruction,
            callee,
            receiver,
            &arguments,
            Some(target_callee.plan),
            destination,
        );
        true
    }

    /// A named read of the method the site's receiver inherits, once the
    /// receiver is proved to have the hidden class the method's guard names:
    /// the guarded holder slot, which a call of the read value then enters
    /// directly or splices.
    fn load_receiver_method(&mut self, instruction: &Instruction) -> bool {
        let byte_pc = instruction.byte_pc;
        if self.exited_for(ExitReason::ShapeGuard) || self.exited_for(ExitReason::IdentityGuard) {
            return false;
        }
        let view: &'a JitCompileSnapshot = self.view;
        let Some((target, method, _)) = view
            .method_targets
            .get(&byte_pc)
            .and_then(|methods| single_method_target(methods))
        else {
            return false;
        };
        if method.guard.accessor_byte.is_some() {
            return false;
        }
        let object = self.read(instruction.reads[0]);
        let object = self.tagged(object);
        if self.known_shapes(object).as_deref() != Some(&[method.guard.recv_shape][..]) {
            return false;
        }
        let value = self.guarded_method(byte_pc, target, true, method, object);
        self.write(instruction.writes[0], value);
        true
    }

    /// The read of the method target `target` of the guarded site at
    /// `byte_pc`. Once the receiver is proved, a method a code-retained cell
    /// holds depends on nothing but the chain facts: that read takes no
    /// input, so it hoists out of loops and merges with its equals.
    fn guarded_method(
        &mut self,
        byte_pc: u32,
        target: u8,
        receiver_proved: bool,
        method: &otter_vm::jit::JitMethodTarget,
        receiver: NodeId,
    ) -> NodeId {
        let cell_read = reads_method_cell(method);
        let kind = Kind::LoadGuardedMethod {
            byte_pc,
            target,
            receiver_proved,
            cell_read,
        };
        if receiver_proved && cell_read {
            self.add(kind, &[], Repr::Tagged)
        } else {
            self.add(kind, &[receiver], Repr::Tagged)
        }
    }

    /// The direct method a read of this body's guarded method `node` names.
    fn loaded_method(
        &self,
        node: NodeId,
    ) -> Option<(
        &'a otter_vm::jit::JitMethodTarget,
        &'a otter_vm::jit::JitDirectCallee,
    )> {
        let data = self.graph.node(node);
        let Kind::LoadGuardedMethod {
            byte_pc, target, ..
        } = data.kind
        else {
            return None;
        };
        if data.origin != self.graph.origin {
            return None;
        }
        let view: &'a JitCompileSnapshot = self.view;
        let method = view
            .method_targets
            .get(&byte_pc)?
            .get(usize::from(target))?;
        Some((method, method.callee.as_ref()?))
    }

    /// `dst = callee.apply(this, arguments)` over the activation's own
    /// `arguments`, never materialized: once the method is proved to be
    /// `%Function.prototype.apply%` and no arguments object exists, a call
    /// of `callee` with the activation's actual arguments, each mapped
    /// formal read from where it lives now. A spliced body passes its
    /// call's argument values.
    fn visit_call_forward(&mut self, instruction: &Instruction) {
        let (Some(method), Some(callee), Some(this)) = (
            instruction.register(1),
            instruction.register(2),
            instruction.register(3),
        ) else {
            return self.generic(instruction);
        };
        let Some(apply) = self.view.forward_apply_native_ref else {
            return self.generic(instruction);
        };
        if self.exited_for(ExitReason::IdentityGuard) {
            return self.generic(instruction);
        }
        if !self.view.instructions[self.pc as usize].call_attempted {
            return self.deopt(DeoptReason::InsufficientFeedback);
        }
        if let Some(actuals) = self
            .inline
            .as_ref()
            .and_then(|inline| inline.caller.arguments.clone())
        {
            return self.emit_spliced_forward(instruction, (method, callee, this), apply, &actuals);
        }
        if self.inline.is_some() {
            return self.generic(instruction);
        }
        let method = self.read(method);
        let method = self.tagged(method);
        self.add(
            Kind::CheckNative(apply, DeoptReason::WrongValue),
            &[method],
            Repr::None,
        );
        self.add(Kind::CheckArgumentsElided, &[], Repr::None);
        let callee = self.read(callee);
        let callee = self.tagged(callee);
        let this = self.read(this);
        let this = self.tagged(this);
        let mut inputs: SmallVec<[NodeId; 4]> = smallvec::smallvec![callee, this];
        let mut bindings: Vec<u16> = Vec::new();
        let slots_byte = self.view.context_layout.slots_byte as i32;
        let view = self.view;
        for (argument_index, storage) in view.code_block.forwarded_argument_bindings() {
            let value = match storage {
                otter_bytecode::ArgumentBindingStorage::Register { reg } => {
                    let value = self.read(reg);
                    self.tagged(value)
                }
                otter_bytecode::ArgumentBindingStorage::Context { reg, slot } => {
                    let context = self.read(reg);
                    let context = self.tagged(context);
                    let offset = slots_byte + i32::from(slot) * 8;
                    self.add(Kind::LoadTaggedField(offset), &[context], Repr::Tagged)
                }
            };
            inputs.push(value);
            bindings.push(argument_index);
        }
        let plan = self
            .view
            .direct_callees
            .get(&instruction.byte_pc)
            .filter(|targets| targets.len() == 1)
            .map(|targets| targets[0].plan);
        let node = self.add(
            Kind::CallForward {
                pc: self.pc,
                plan: crate::call_linkage::CallPlan::for_site(self.view, instruction.byte_pc, plan),
                bindings: bindings.into_boxed_slice(),
                skip: 0,
            },
            &inputs,
            Repr::Tagged,
        );
        let destination = instruction.writes[0];
        let lazy = self.lazy_state(Some((destination, node)));
        self.graph.node_mut(node).lazy = Some(lazy);
        self.write(destination, node);
    }

    /// `callee.apply(this, arguments)` in a spliced body, whose actual
    /// arguments are the call's `actuals`: once `apply` is proved, the call
    /// of `callee` with those values, each mapped formal's current value in
    /// place of its actual, which a known target may splice in turn.
    fn emit_spliced_forward(
        &mut self,
        instruction: &Instruction,
        (method, callee, this): (u16, u16, u16),
        apply: u32,
        actuals: &[NodeId],
    ) {
        let method = self.read(method);
        let method = self.tagged(method);
        self.add(
            Kind::CheckNative(apply, DeoptReason::WrongValue),
            &[method],
            Repr::None,
        );
        let callee = self.read(callee);
        let callee = self.tagged(callee);
        let this = self.read(this);
        let this = self.tagged(this);
        let mut arguments: SmallVec<[NodeId; 8]> = actuals.iter().copied().collect();
        let slots_byte = self.view.context_layout.slots_byte as i32;
        let view = self.view;
        for (argument_index, storage) in view.code_block.forwarded_argument_bindings() {
            let index = usize::from(argument_index);
            if index >= arguments.len() {
                continue;
            }
            arguments[index] = match storage {
                otter_bytecode::ArgumentBindingStorage::Register { reg } => {
                    let value = self.read(reg);
                    self.tagged(value)
                }
                otter_bytecode::ArgumentBindingStorage::Context { reg, slot } => {
                    let context = self.read(reg);
                    let context = self.tagged(context);
                    let offset = slots_byte + i32::from(slot) * 8;
                    self.add(Kind::LoadTaggedField(offset), &[context], Repr::Tagged)
                }
            };
        }
        for argument in &mut arguments {
            *argument = self.tagged(*argument);
        }
        // A method read through its holder slot names its target.
        if let Some((method, target)) = self.loaded_method(callee) {
            let plan = target.plan;
            if let Some(body) = method.body.as_ref()
                && self.binds_this_as_is(body, plan, this)
                && self.inline_call(instruction, body, plan, callee, this, &arguments)
            {
                return;
            }
            let destination = Some(instruction.writes[0]);
            return self.emit_receiver_call(
                instruction,
                callee,
                this,
                &arguments,
                Some(plan),
                destination,
            );
        }
        let plan = self
            .view
            .direct_callees
            .get(&instruction.byte_pc)
            .filter(|targets| targets.len() == 1)
            .map(|targets| targets[0].plan);
        self.emit_call_with_receiver(instruction, callee, this, &arguments, plan);
    }

    /// `f.call(thisArg, ...args)` once `%Function.prototype.call%` is proved:
    /// the call of `f` with `thisArg` as receiver.
    fn emit_function_prototype_call(
        &mut self,
        instruction: &Instruction,
        function: NodeId,
        arguments: &[NodeId],
        plan: Option<otter_vm::jit::JitDirectCallPlan>,
    ) {
        let (this, rest) = match arguments.split_first() {
            Some((&this, rest)) => (this, rest),
            None => (self.undefined, &[][..]),
        };
        let rest: SmallVec<[NodeId; 8]> = rest.iter().copied().collect();
        self.emit_call_with_receiver(instruction, function, this, &rest, plan);
    }

    /// `dst = callee.call(this, args...)` as the bytecode spells a call with
    /// an explicit receiver.
    fn visit_call_with_this(&mut self, instruction: &Instruction) {
        let (Some(callee), Some(this), Some(arguments)) = (
            instruction.register(1),
            instruction.register(2),
            Self::call_arguments(instruction, 3),
        ) else {
            return self.generic(instruction);
        };
        if !self.view.instructions[self.pc as usize].call_attempted {
            return self.deopt(DeoptReason::InsufficientFeedback);
        }
        // The loaded callable and actuals already belong to this instruction.
        // Only a passive exact-arity declaration can leave before this call;
        // mutating and unsupported leaf shapes keep the baseline operation.
        let leaf = self
            .view
            .native_calls
            .get(&instruction.byte_pc)
            .is_some_and(|target| target.leaf().is_some());
        let pure = self.admitted_native_leaf(instruction, arguments.len());
        if leaf && pure.is_none() {
            return self.generic(instruction);
        }
        let callee = self.read(callee);
        let callee = self.tagged(callee);
        let this = self.read(this);
        let this = self.tagged(this);
        let arguments: SmallVec<[NodeId; 8]> = arguments
            .iter()
            .map(|&register| {
                let value = self.read(register);
                self.tagged(value)
            })
            .collect();
        if let Some((target, declaration)) = pure {
            return self.emit_native_leaf_call(
                instruction,
                target,
                declaration,
                callee,
                this,
                &arguments,
            );
        }
        // A loaded `f.call` called with `f` as receiver calls `f`; once the
        // site left on a different `f`, whichever `f` it gets.
        if let Some(site) = self.view.function_prototype_calls.get(&instruction.byte_pc)
            && site.proof.lookup.is_none()
            && !self.exited_for(ExitReason::TypeMismatch)
        {
            let identity_left = self.exited_for(ExitReason::IdentityGuard);
            let plan = site
                .callee
                .filter(|callee| !identity_left || self.constant_closure_of(this, callee.plan))
                .map(|callee| callee.plan);
            // After an identity exit only the intrinsic is checked, with an
            // exit of its own: a second exit keeps the ordinary call.
            let check = if identity_left {
                Kind::CheckNative(site.proof.call_native_ref, DeoptReason::WrongType)
            } else {
                Kind::CheckFunctionPrototypeCall(instruction.byte_pc)
            };
            self.add(check, &[callee], Repr::None);
            return self.emit_function_prototype_call(instruction, this, &arguments, plan);
        }
        let plan = self
            .view
            .direct_callees
            .get(&instruction.byte_pc)
            .filter(|targets| targets.len() == 1)
            .map(|targets| targets[0].plan);
        self.emit_call_with_receiver(instruction, callee, this, &arguments, plan);
    }

    /// The passive exact-arity leaf a call site saw, when this site may still
    /// speculate on its callee's identity.
    fn admitted_native_leaf(
        &self,
        instruction: &Instruction,
        argument_count: usize,
    ) -> Option<(
        otter_vm::jit::JitStaticNativeCall,
        &'static otter_vm::jit_static_native::JitLeafBuiltin,
    )> {
        let target = *self.view.native_calls.get(&instruction.byte_pc)?.leaf()?;
        if self.exited_for(ExitReason::IdentityGuard) {
            return None;
        }
        super::native_leaf::admit(self.view, target, argument_count)
            .map(|declaration| (target, declaration))
    }

    /// Call a proved static native through its leaf on the operand words,
    /// without a frame: the callee is checked to be that native first.
    fn emit_native_leaf_call(
        &mut self,
        instruction: &Instruction,
        target: otter_vm::jit::JitStaticNativeCall,
        declaration: &otter_vm::jit_static_native::JitLeafBuiltin,
        callee: NodeId,
        this: NodeId,
        arguments: &[NodeId],
    ) {
        self.add(
            Kind::CheckNative(target.builtin_native_ref, DeoptReason::WrongValue),
            &[callee],
            Repr::None,
        );
        // A `Math` method of Numbers is the float64 arithmetic itself: no
        // box, no leaf frame, one instruction where the target has it. An
        // operand not proved a Number is checked once; after such a check
        // failed, the recompile calls the leaf.
        if let Some(op) = super::ir::MathOp::of_leaf(target.leaf_stub_id)
            && arguments.len() == op.arity()
            && (!self.exited_for(ExitReason::TypeMismatch)
                || arguments.iter().all(|&argument| self.is_number(argument)))
        {
            let mut inputs: SmallVec<[NodeId; 2]> = SmallVec::new();
            for &argument in arguments {
                inputs.push(self.float64(argument));
            }
            let result = self.add(Kind::Float64Math(op), &inputs, Repr::Float64);
            self.write(instruction.writes[0], result);
            return;
        }
        // `String.prototype.charCodeAt` of a flat string at an index within
        // it reads the code unit inline; anything else leaves, and the
        // recompile after such an exit calls the leaf.
        if target.leaf_stub_id == otter_vm::native_abi::STUB_STRING_CHAR_CODE_AT_LEAF.id
            && declaration.this_operand
            && arguments.len() == 1
            && !self.exited_for(ExitReason::TypeMismatch)
            && !self.exited_for(ExitReason::BoundsGuard)
        {
            let index = self.int32(arguments[0]);
            let unit = self.add(Kind::StringCharCodeAt, &[this, index], Repr::Int32);
            self.write(instruction.writes[0], unit);
            return;
        }
        let mut inputs: SmallVec<[NodeId; 2]> = SmallVec::new();
        if declaration.this_operand {
            inputs.push(this);
        }
        inputs.extend(arguments.iter().copied());
        inputs.resize(2, self.undefined);
        let node = self.add(Kind::NativeLeaf(target.leaf_stub_id), &inputs, Repr::Tagged);
        self.write(instruction.writes[0], node);
    }

    /// `dst = receiver.name(args...)`: `f.call` on a closure receiver, a
    /// guarded method of a proved receiver, a method read like a named load
    /// and called through the generic entry, or the baseline operation.
    fn visit_call_method(&mut self, instruction: &Instruction) {
        let (Some(receiver), Some(arguments)) = (
            instruction.register(1),
            Self::call_arguments(instruction, 3),
        ) else {
            return self.generic(instruction);
        };
        if !self.view.instructions[self.pc as usize].call_attempted {
            return self.deopt(DeoptReason::InsufficientFeedback);
        }
        if self.emit_array_method(instruction, receiver, &arguments)
            || self.emit_string_method(instruction, receiver, &arguments)
        {
            return;
        }
        let byte_pc = instruction.byte_pc;
        // `Reflect.apply(target, this, list)` with the realm's original
        // `apply`: the method is read like a named load, proved, and the
        // call spreads the list.
        if let Some(&otter_vm::JitNativeCall::Intrinsic(proof)) =
            self.view.native_calls.get(&byte_pc)
            && proof.kind == otter_vm::IntrinsicCallKind::ReflectApply
            && !self.exited_for(ExitReason::IdentityGuard)
            && arguments.len() == 3
        {
            let namespace = self.read(receiver);
            let namespace = self.tagged(namespace);
            let method = self.named_lookup(byte_pc, namespace);
            if self.emit_reflect_apply_value(instruction, proof, method, &arguments) {
                return;
            }
        }
        let fold = (!self.exited_for(ExitReason::IdentityGuard))
            .then(|| self.view.function_prototype_calls.get(&byte_pc))
            .flatten()
            .filter(|site| {
                site.proof
                    .lookup
                    .is_some_and(|lookup| lookup.receiver.is_generated_receiver())
            })
            .map(|site| site.callee.map(|callee| callee.plan));
        // A `f.call(...)` site whose receiver proof left (a function with own
        // properties, or a different applied function) reads `call` like a
        // named load and checks it is the intrinsic; that check leaving too
        // keeps the ordinary method call.
        if fold.is_none()
            && self.exited_for(ExitReason::IdentityGuard)
            && !self.exited_for(ExitReason::TypeMismatch)
            && let Some(site) = self.view.function_prototype_calls.get(&byte_pc)
        {
            let call_native_ref = site.proof.call_native_ref;
            let function = self.read(receiver);
            let function = self.tagged(function);
            let plan = site
                .callee
                .filter(|callee| self.constant_closure_of(function, callee.plan))
                .map(|callee| callee.plan);
            let method = self.named_lookup(byte_pc, function);
            self.add(
                Kind::CheckNative(call_native_ref, DeoptReason::WrongType),
                &[method],
                Repr::None,
            );
            let arguments: SmallVec<[NodeId; 8]> = arguments
                .iter()
                .map(|&register| {
                    let value = self.read(register);
                    self.tagged(value)
                })
                .collect();
            return self.emit_function_prototype_call(instruction, function, &arguments, plan);
        }
        let view: &'a JitCompileSnapshot = self.view;
        let methods = (!self.exited_for(ExitReason::ShapeGuard)
            && !self.exited_for(ExitReason::IdentityGuard))
        .then(|| view.method_targets.get(&byte_pc))
        .flatten();
        let monomorphic = methods.and_then(|methods| single_method_target(methods));
        let method = monomorphic.map(|(_, _, callee)| callee.plan);
        let polymorphic = methods.filter(|methods| {
            methods
                .iter()
                .filter(|method| method.callee.is_some())
                .count()
                >= 2
        });
        // Any other method is read like a named load when the site's lookup
        // feedback describes it, then called through the generic entry.
        let lookup = self.view.property_programs.contains_key(&byte_pc)
            || self
                .view
                .property_accesses
                .get(&byte_pc)
                .is_some_and(|site| site.shared);
        // A `Math` method the site saw, read like a named load: when the
        // method read is that native, its arithmetic runs on float64
        // operands, so an inlined body stays inlined. Once an operand was not
        // a Number here, the baseline operation takes the call, as it does
        // for every other leaf method, whose misses it commits.
        // A guard on the namespace's shape proves the same native the named
        // read checks; an exotic receiver's guard names a builtin of its own.
        let exotic_guard = self
            .view
            .guarded_method_calls
            .get(&byte_pc)
            .is_some_and(|call| {
                !matches!(
                    call.receiver,
                    otter_vm::jit::JitGuardedReceiver::Shape { .. }
                )
            });
        if lookup
            && fold.is_none()
            && method.is_none()
            && polymorphic.is_none()
            && !exotic_guard
            && !self.exited_for(ExitReason::TypeMismatch)
            && let Some((target, _)) = self.admitted_native_leaf(instruction, arguments.len())
            && let Some(op) = super::ir::MathOp::of_leaf(target.leaf_stub_id)
            && arguments.len() == op.arity()
        {
            let receiver = self.read(receiver);
            let receiver = self.tagged(receiver);
            let arguments: SmallVec<[NodeId; 8]> = arguments
                .iter()
                .map(|&register| {
                    let value = self.read(register);
                    self.tagged(value)
                })
                .collect();
            let callee = self.named_lookup(byte_pc, receiver);
            return self.emit_math_method_call(
                instruction,
                op,
                target.builtin_native_ref,
                callee,
                receiver,
                &arguments,
            );
        }
        // Any other native leaf method is entered by the baseline operation
        // without a frame.
        if (fold.is_none() && method.is_none() && polymorphic.is_none() && !lookup)
            || self
                .view
                .native_calls
                .get(&byte_pc)
                .is_some_and(|target| target.leaf().is_some())
            || self.view.guarded_method_calls.contains_key(&byte_pc)
        {
            return self.generic(instruction);
        }
        let receiver = self.read(receiver);
        let receiver = self.tagged(receiver);
        let arguments: SmallVec<[NodeId; 8]> = arguments
            .iter()
            .map(|&register| {
                let value = self.read(register);
                self.tagged(value)
            })
            .collect();
        if let Some(plan) = fold {
            self.add(
                Kind::CheckFunctionPrototypeCall(byte_pc),
                &[receiver],
                Repr::None,
            );
            return self.emit_function_prototype_call(instruction, receiver, &arguments, plan);
        }
        if let Some((target, method, callee_plan)) = monomorphic {
            let plan = callee_plan.plan;
            // A receiver this path already proved to have the guard's shape
            // needs no second proof.
            let receiver_proved = self
                .known_shapes(receiver)
                .is_some_and(|shapes| shapes.as_slice() == [method.guard.recv_shape]);
            let callee = self.guarded_method(byte_pc, target, receiver_proved, method, receiver);
            self.prove_receiver_shape(receiver, method.guard.recv_shape);
            let body = method.body.as_ref();
            // The guard proved an object receiver, which a non-arrow method
            // binds as `this`.
            if let Some(body) = body
                && (!body.code_block.observes_this() || !body.code_block.is_arrow())
                && self.inline_call(instruction, body, plan, callee, receiver, &arguments)
            {
                return;
            }
            return self.emit_call_with_receiver(
                instruction,
                callee,
                receiver,
                &arguments,
                Some(plan),
            );
        }
        if let Some(methods) = polymorphic {
            return self.emit_polymorphic_method_call(
                instruction,
                receiver,
                &arguments,
                methods,
                true,
            );
        }
        let callee = self.named_lookup(byte_pc, receiver);
        self.emit_call_with_receiver(instruction, callee, receiver, &arguments, None);
    }

    /// A `Math` method call whose method was read like a named load. When
    /// the method read is the site's native, its arithmetic runs on float64
    /// operands; any other method read is called like any method, so a
    /// replaced method neither leaves optimized code nor abandons an inlined
    /// body. The arms meet with the call's result in its register.
    fn emit_math_method_call(
        &mut self,
        instruction: &Instruction,
        op: super::ir::MathOp,
        native_ref: u32,
        callee: NodeId,
        receiver: NodeId,
        arguments: &[NodeId],
    ) {
        let Some(current) = self.current else {
            return;
        };
        let destination = instruction.writes[0];
        let before = self.frame[usize::from(destination)];
        let owner = self.current_slot;
        let entry_known = self.known.clone();
        let native = self.graph.new_block();
        let other = self.graph.new_block();
        let join = self.graph.new_block();
        self.terminate(
            Kind::Branch {
                kind: BranchKind::Native(native_ref),
                if_true: native,
                if_false: other,
            },
            &[callee],
        );
        self.graph.block_mut(native).predecessors.push(current);
        self.graph.block_mut(other).predecessors.push(current);
        let mut arms: SmallVec<[(BlockId, NodeId, Known); 2]> = SmallVec::new();
        let finish_arm =
            |builder: &mut Self, arms: &mut SmallVec<[(BlockId, NodeId, Known); 2]>| {
                if let Some(end) = builder.current {
                    let value = builder.frame[usize::from(destination)];
                    arms.push((end, value, builder.known.clone()));
                    builder.terminate(Kind::Jump(join), &[]);
                }
            };
        self.trailing[owner].push(native);
        self.current = Some(native);
        let mut inputs: SmallVec<[NodeId; 2]> = SmallVec::new();
        for &argument in arguments {
            inputs.push(self.float64(argument));
        }
        let result = self.add(Kind::Float64Math(op), &inputs, Repr::Float64);
        self.write(destination, result);
        finish_arm(self, &mut arms);
        self.trailing[owner].push(other);
        self.current = Some(other);
        self.known = entry_known;
        self.frame[usize::from(destination)] = before;
        self.emit_receiver_call(
            instruction,
            callee,
            receiver,
            arguments,
            None,
            Some(destination),
        );
        finish_arm(self, &mut arms);
        if arms.is_empty() {
            // Both arms left: nothing reaches the join.
            return;
        }
        self.trailing[owner].push(join);
        self.graph.block_mut(join).predecessors = arms.iter().map(|&(block, _, _)| block).collect();
        let mut known = arms[0].2.clone();
        for (_, _, edge) in &arms[1..] {
            known.intersect(edge);
        }
        self.current = Some(join);
        let inputs: SmallVec<[NodeId; 4]> = arms
            .iter_mut()
            .map(|(block, value, edge)| self.tagged_at_end(*block, *value, edge))
            .collect();
        let phi = self.graph.add_node(Kind::Phi, &inputs, Repr::Tagged);
        self.graph.node_mut(phi).block = Some(join);
        self.graph.block_mut(join).phis.push(phi);
        self.known = known;
        self.write(destination, phi);
    }

    /// A method call whose receivers had several observed shapes: the
    /// receiver's shape selects one arm per target, which reads that
    /// target's guarded method and runs it inline (when `splice` admits it)
    /// or calls it directly; a receiver of no listed shape leaves optimized
    /// code. The arms meet with the call's result in its register. A getter
    /// read through a named load is called, never spliced: its frame resumes
    /// only through a call instruction.
    fn emit_polymorphic_method_call(
        &mut self,
        instruction: &Instruction,
        receiver: NodeId,
        arguments: &[NodeId],
        methods: &'a [otter_vm::jit::JitMethodTarget],
        splice: bool,
    ) {
        let byte_pc = instruction.byte_pc;
        let destination = instruction.writes[0];
        let before = self.frame[usize::from(destination)];
        let owner = self.current_slot;
        let shape = self.add(Kind::LoadReceiverShape, &[receiver], Repr::Word);
        let join = self.graph.new_block();
        let entry_known = self.known.clone();
        let mut arms: Vec<(BlockId, NodeId, Known)> = Vec::new();
        let finish_arm = |builder: &mut Self, arms: &mut Vec<(BlockId, NodeId, Known)>| {
            if let Some(end) = builder.current {
                let value = builder.frame[usize::from(destination)];
                arms.push((end, value, builder.known.clone()));
                builder.terminate(Kind::Jump(join), &[]);
            }
        };
        // Enumerate the canonical vector before skipping Template-only
        // targets: LoadGuardedMethod names that original vector position.
        for (index, method) in methods.iter().enumerate() {
            let Some(target) = method.callee.as_ref() else {
                continue;
            };
            let Some(current) = self.current else {
                break;
            };
            let matched = self.graph.new_block();
            let next = self.graph.new_block();
            self.terminate(
                Kind::Branch {
                    kind: BranchKind::WordEqual(method.guard.recv_shape),
                    if_true: matched,
                    if_false: next,
                },
                &[shape],
            );
            self.graph.block_mut(matched).predecessors.push(current);
            self.graph.block_mut(next).predecessors.push(current);
            self.trailing[owner].push(matched);
            self.current = Some(matched);
            self.known = entry_known.clone();
            self.frame[usize::from(destination)] = before;
            let callee = self.guarded_method(byte_pc, index as u8, true, method, receiver);
            self.prove_receiver_shape(receiver, method.guard.recv_shape);
            let plan = target.plan;
            let inlined = splice
                && method.body.as_ref().is_some_and(|body| {
                    (!body.code_block.observes_this() || !body.code_block.is_arrow())
                        && self.inline_call(instruction, body, plan, callee, receiver, arguments)
                });
            if !inlined {
                self.emit_call_with_receiver(
                    instruction,
                    callee,
                    receiver,
                    arguments,
                    Some(target.plan),
                );
            }
            finish_arm(self, &mut arms);
            self.trailing[owner].push(next);
            self.current = Some(next);
        }
        self.known = entry_known;
        self.frame[usize::from(destination)] = before;
        // The site's feedback names every receiver shape it saw: one it did
        // not leaves, like the monomorphic guard, and the recompile after
        // such an exit reads the method like a named load.
        self.deopt(DeoptReason::WrongShape);
        if arms.is_empty() {
            // Every arm left: nothing reaches the join.
            return;
        }
        self.trailing[owner].push(join);
        self.graph.block_mut(join).predecessors = arms.iter().map(|&(block, _, _)| block).collect();
        let mut known = arms[0].2.clone();
        for (_, _, edge) in &arms[1..] {
            known.intersect(edge);
        }
        self.current = Some(join);
        let bound = field_type::merged_bound(
            &self.graph,
            &self.root.stable_shapes,
            arms.iter().map(|(_, value, known)| (*value, known)),
        );
        let inputs: SmallVec<[NodeId; 4]> = arms
            .iter_mut()
            .map(|(block, value, edge)| self.tagged_at_end(*block, *value, edge))
            .collect();
        let phi = self.graph.add_node(Kind::Phi, &inputs, Repr::Tagged);
        self.graph.node_mut(phi).block = Some(join);
        self.graph.block_mut(join).phis.push(phi);
        self.known = known;
        if let Some(bound) = bound {
            self.known.field_values.insert(phi, bound);
        }
        self.write(destination, phi);
    }

    // ------------------------------------------------------------------
    // Indexed element access
    // ------------------------------------------------------------------

    /// The element layout this site speculates on, or why it has none:
    /// `Err(true)` for a site that never ran, `Err(false)` for one whose
    /// receivers have no single layout, a store site that reached past its
    /// receiver's elements, or one whose receiver, bounds or key speculation
    /// already failed.
    fn element_access(&self, instruction: &Instruction) -> Result<JitElementAccess, bool> {
        let byte_pc = instruction.byte_pc;
        if self.view.cage_base == 0 {
            return Err(false);
        }
        if self.view.unseen_element_sites.contains(&byte_pc) {
            return Err(true);
        }
        let Some(&access) = self.view.element_accesses.get(&byte_pc) else {
            return Err(false);
        };
        // A load whose absent indices read `undefined` in place keeps its
        // specialized access; any other site that met them stays generic.
        if access.type_tag == 0
            || matches!(access.base, JitElementBase::None)
            || (self.view.out_of_bounds_element_sites.contains(&byte_pc) && !access.reads_absent)
            || self.exited_for(ExitReason::ShapeGuard)
            || self.exited_for(ExitReason::BoundsGuard)
            || self.exited_for(ExitReason::InvalidElementIndex)
        {
            return Err(false);
        }
        Ok(access)
    }

    /// Give the current block's check that proved `known` for `receiver`
    /// the stronger guards of `access`, when no node after it writes memory:
    /// failing earlier then resumes at a point with nothing committed since.
    fn strengthen_element_check(
        &mut self,
        receiver: NodeId,
        known: &JitElementAccess,
        access: &JitElementAccess,
    ) -> bool {
        let block = self.current.expect("an open block");
        let body = self.graph.block(block).body.clone();
        for &node in body.iter().rev() {
            let data = self.graph.node(node);
            if let Kind::CheckElements { guards, .. } = &data.kind
                && data.inputs.as_slice() == [receiver]
                && *guards == known.guards
            {
                let Kind::CheckElements { guards, .. } = &mut self.graph.node_mut(node).kind else {
                    unreachable!("matched an element check");
                };
                *guards = access.guards;
                return true;
            }
            let properties = data.kind.properties();
            if properties.call
                || properties.may_collect
                || !super::effects::clobberize(&data.kind).writes.is_empty()
            {
                return false;
            }
        }
        false
    }

    /// Prove `receiver` has `access`'s layout and read its storage, unless
    /// this path already did. A typed view is proved to keep a cached
    /// element base, which its storage then reads directly.
    fn elements_of(&mut self, receiver: NodeId, access: JitElementAccess) -> ElementStorage {
        let known = self
            .known
            .layout(receiver)
            .and_then(|layout| layout.elements);
        let proved = known.is_some_and(|known| known.implies(&access));
        if proved && let Some(&storage) = self.known.storage.get(&receiver) {
            return storage;
        }
        // A weaker proof of the same addressing keeps its storage words;
        // only the stronger guards still need checking.
        if let Some(known) = known
            && known.addresses_like(&access)
            && access.implies(&known)
            && let Some(&storage) = self.known.storage.get(&receiver)
        {
            if !self.strengthen_element_check(receiver, &known, &access) {
                self.add(
                    Kind::CheckElements {
                        type_tag: access.type_tag,
                        guards: access.guards,
                        holes: access.holes,
                        cached_base: match access.base {
                            JitElementBase::ThroughLocalBuffer {
                                cached_data_byte, ..
                            } => Some(cached_data_byte),
                            _ => None,
                        },
                    },
                    &[receiver],
                    Repr::None,
                );
            }
            self.known.layout_entry(receiver).elements = Some(access);
            return storage;
        }
        let (base_byte, cached_base) = match access.base {
            JitElementBase::InBody { byte } => (byte, None),
            JitElementBase::ThroughLocalBuffer {
                cached_data_byte, ..
            } => (cached_data_byte, Some(cached_data_byte)),
            JitElementBase::None => unreachable!("an element access names its base"),
        };
        if !proved {
            self.add(
                Kind::CheckElements {
                    type_tag: access.type_tag,
                    guards: access.guards,
                    holes: access.holes,
                    cached_base,
                },
                &[receiver],
                Repr::None,
            );
        }
        let length = self.add(
            Kind::LoadElementsLength {
                byte: access.length_byte,
                width: access.length_width,
            },
            &[receiver],
            Repr::Word,
        );
        let base = self.add(
            Kind::LoadElementsBase {
                byte: base_byte,
                off_heap: cached_base.is_some(),
            },
            &[receiver],
            Repr::Word,
        );
        let storage = ElementStorage { length, base };
        // A stronger proof already on this path stays: it serves both.
        self.known.layout_entry(receiver).elements = Some(if proved {
            known.expect("proved layout")
        } else {
            access
        });
        self.known.storage.insert(receiver, storage);
        self.known.entry(receiver).heap_object = true;
        storage
    }

    /// The proved storage of tagged `receiver` and the in-bounds int32 form
    /// of register `index`.
    fn element_address(
        &mut self,
        receiver: NodeId,
        index: u16,
        access: JitElementAccess,
    ) -> (ElementStorage, NodeId) {
        let index = self.read(index);
        let index = self.element_index(index);
        let fact = self.elements_of(receiver, access);
        if self.known.bounds.insert((index, fact.length)) {
            self.add(Kind::CheckBounds, &[index, fact.length], Repr::None);
        }
        (fact, index)
    }

    /// The int32 form of an element key: any Number holding an integer in
    /// the int32 range. Range and sign are the bounds check's.
    fn element_index(&mut self, index: NodeId) -> NodeId {
        let node = self.graph.node(index);
        match (&node.kind, node.repr) {
            (_, Repr::Int32) => return index,
            (Kind::ConstTagged(bits), _) if bits & tag::NUMBER_TAG == tag::NUMBER_TAG => {
                let int = *bits as u32 as i32;
                return self.constant_int32(int);
            }
            _ => {}
        }
        if let Some(int) = self.known.get(index).and_then(|info| info.int32) {
            return int;
        }
        // Not recorded as the value's int32 form: `-0` converts to `0`.
        match self.graph.node(index).repr {
            Repr::Float64 => self.add(Kind::CheckedFloat64ToIndex, &[index], Repr::Int32),
            _ => self.add(Kind::CheckedTaggedToIndex, &[index], Repr::Int32),
        }
    }

    /// Prove the key in `register` is `name`: a symbol by its atom, an index
    /// by its int32 bits, which a constant key already is.
    fn check_keyed_name(&mut self, register: u16, name: otter_vm::jit::JitKeyedName) {
        let key = self.read(register);
        let key = self.tagged(key);
        match name {
            otter_vm::jit::JitKeyedName::Symbol { atom } => {
                self.add(Kind::CheckSymbolAtom(atom), &[key], Repr::None);
            }
            otter_vm::jit::JitKeyedName::Index { index } => {
                let bits = otter_vm::Value::number_i32(index).to_bits();
                if !matches!(self.graph.node(key).kind, Kind::ConstTagged(constant) if constant == bits)
                {
                    self.add(Kind::CheckTaggedBits(bits), &[key], Repr::None);
                }
            }
        }
    }

    /// An element site whose cache serves one name key reads it as a named
    /// load does, after proving the key is that name.
    fn keyed_name_load(&mut self, instruction: &Instruction) -> bool {
        let byte_pc = instruction.byte_pc;
        let Some(&name) = self.view.keyed_names.get(&byte_pc) else {
            return false;
        };
        if self.exited_for(ExitReason::ShapeGuard) || self.exited_for(ExitReason::IdentityGuard) {
            return false;
        }
        let receiver = self.read(instruction.reads[0]);
        let receiver = self.tagged(receiver);
        let receivers = self.known_shapes(receiver);
        if super::feedback::named_load_programs(self.view, byte_pc, receivers.as_deref()).is_none()
        {
            return false;
        }
        self.check_keyed_name(instruction.reads[1], name);
        let value = self.named_lookup(byte_pc, receiver);
        self.write(instruction.writes[0], value);
        true
    }

    /// An element site whose cache serves one name key writes it as a named
    /// store does, after proving the key is that name.
    fn keyed_name_store(&mut self, instruction: &Instruction) -> bool {
        let byte_pc = instruction.byte_pc;
        let Some(&name) = self.view.keyed_names.get(&byte_pc) else {
            return false;
        };
        if self.exited_for(ExitReason::ShapeGuard) || self.exited_for(ExitReason::IdentityGuard) {
            return false;
        }
        if super::feedback::named_store_programs(self.view, byte_pc).is_none() {
            return false;
        }
        self.check_keyed_name(instruction.reads[1], name);
        self.visit_store_property_regs(instruction, instruction.reads[0], instruction.reads[2]);
        true
    }

    fn visit_load_element(&mut self, instruction: &Instruction) {
        if self.keyed_name_load(instruction) {
            return;
        }
        let access = match self.element_access(instruction) {
            Ok(access) => access,
            Err(true) => return self.deopt(DeoptReason::InsufficientFeedback),
            Err(false) => return self.keyed_load(instruction),
        };
        let receiver = self.read(instruction.reads[0]);
        let receiver = self.tagged(receiver);
        if access.reads_absent {
            return self.load_element_or_undefined(instruction, receiver, access);
        }
        let (fact, index) = self.element_address(receiver, instruction.reads[1], access);
        // A boxed read proves an own slot as it reads: the descriptor admits
        // a prototype-only sidecar, so a hole must use committed prototype
        // lookup.
        let kind = match access.element {
            JitElementRepr::Boxed => Kind::LoadPresentElement,
            JitElementRepr::Float64 if let Some(holes) = access.holes => {
                Kind::LoadHoleyFloat64Element(holes)
            }
            JitElementRepr::Uint32 if self.exited_for(ExitReason::TypeMismatch) => {
                Kind::LoadElementUint32ToFloat64
            }
            element => Kind::LoadElement(element),
        };
        // The element read once on this path, with nothing stored since.
        if let Some((read, value)) = self.known.elements_read.get(&(fact.base, index))
            && *read == kind
        {
            let value = *value;
            self.write(instruction.writes[0], value);
            return;
        }
        let value = match access.element {
            JitElementRepr::Float64 if let Some(holes) = access.holes => self.add(
                Kind::LoadHoleyFloat64Element(holes),
                &[fact.base, index],
                Repr::Tagged,
            ),
            JitElementRepr::Boxed => {
                self.add(Kind::LoadPresentElement, &[fact.base, index], Repr::Tagged)
            }
            JitElementRepr::Float32 | JitElementRepr::Float64 => self.add(
                Kind::LoadElement(access.element),
                &[fact.base, index],
                Repr::Float64,
            ),
            // A `Uint32` element that once left the int32 range is read as a
            // double.
            JitElementRepr::Uint32 if self.exited_for(ExitReason::TypeMismatch) => self.add(
                Kind::LoadElementUint32ToFloat64,
                &[fact.base, index],
                Repr::Float64,
            ),
            element => self.add(Kind::LoadElement(element), &[fact.base, index], Repr::Int32),
        };
        self.known
            .elements_read
            .insert((fact.base, index), (kind, value));
        self.write(instruction.writes[0], value);
    }

    /// A read of a plain Array whose site met holes or indices past the
    /// elements: no bounds check, the element or `undefined` from one node.
    fn load_element_or_undefined(
        &mut self,
        instruction: &Instruction,
        receiver: NodeId,
        access: JitElementAccess,
    ) {
        let index = self.read(instruction.reads[1]);
        let index = self.element_index(index);
        let fact = self.elements_of(receiver, access);
        let kind = Kind::LoadElementOrUndefined;
        if let Some((read, value)) = self.known.elements_read.get(&(fact.base, index))
            && *read == kind
        {
            let value = *value;
            self.write(instruction.writes[0], value);
            return;
        }
        let value = self.add(
            Kind::LoadElementOrUndefined,
            &[fact.base, index, fact.length],
            Repr::Tagged,
        );
        self.known
            .elements_read
            .insert((fact.base, index), (kind, value));
        self.write(instruction.writes[0], value);
    }

    fn visit_store_element(&mut self, instruction: &Instruction) {
        if self.keyed_name_store(instruction) {
            return;
        }
        let access = match self.element_access(instruction) {
            Ok(access) => access,
            Err(true) => return self.deopt(DeoptReason::InsufficientFeedback),
            Err(false) => return self.keyed_store(instruction),
        };
        let value = self.read(instruction.reads[2]);
        let element = access.element;
        // A numeric element stores Numbers only: a site that stored anything
        // else keeps the runtime path.
        let feedback = self.feedback();
        if element != JitElementRepr::Boxed
            && !feedback.is_unseen()
            && !feedback.is_numeric_only()
            && !self.is_number(value)
        {
            return self.generic(instruction);
        }
        // Convert the stored value before the receiver is proved, so the
        // conversion's deopt resumes with nothing written.
        let stored = match element {
            JitElementRepr::Boxed => self.tagged(value),
            JitElementRepr::Float32 | JitElementRepr::Float64 => self.float64(value),
            JitElementRepr::Uint8Clamped => {
                if self.graph.node(value).repr == Repr::Float64 || !self.int32_value(value) {
                    self.float64(value)
                } else {
                    self.int32(value)
                }
            }
            _ => {
                if self.int32_value(value) {
                    self.int32(value)
                } else {
                    let double = self.float64(value);
                    self.add(Kind::TruncateFloat64ToInt32, &[double], Repr::Int32)
                }
            }
        };
        let receiver = self.read(instruction.reads[0]);
        let receiver = self.tagged(receiver);
        let (fact, index) = self.element_address(receiver, instruction.reads[1], access);
        // An absent element would have to consult the prototype chain.
        if element == JitElementRepr::Boxed {
            self.add(Kind::CheckElementPresent, &[fact.base, index], Repr::None);
        } else if let Some(holes) = access.holes {
            self.add(
                Kind::CheckHoleyElementPresent(holes),
                &[fact.base, index],
                Repr::None,
            );
        }
        self.add(
            Kind::StoreElement(element),
            &[fact.base, index, stored],
            Repr::None,
        );
        // Views of one buffer alias: no element read on this path survives.
        self.known.elements_read.clear();
        if element == JitElementRepr::Boxed {
            self.add(
                Kind::ElementWriteBarrier,
                &[fact.base, index, stored],
                Repr::None,
            );
        }
    }

    /// An element store that sees Array storage of several kinds, or whose
    /// speculation left for a bound, a hole or another layout: the generic
    /// keyed store, without a deopt. A typed-array site keeps its baseline
    /// operation.
    fn keyed_store(&mut self, instruction: &Instruction) {
        if instruction.op != Op::StoreElement || !self.keyed_generic_site(instruction) {
            return self.generic(instruction);
        }
        let receiver = self.read(instruction.reads[0]);
        let receiver = self.tagged(receiver);
        let key = self.read(instruction.reads[1]);
        let key = self.tagged(key);
        let value = self.read(instruction.reads[2]);
        let value = self.tagged(value);
        self.add(
            Kind::StoreKeyedCached { pc: self.pc },
            &[receiver, key, value],
            Repr::None,
        );
        self.known.elements_read.clear();
    }

    /// The elided `arguments.length`: a spliced call's actual count, or the
    /// physical activation's.
    fn visit_arguments_length(&mut self, instruction: &Instruction) {
        let count = match self.inline.as_ref().map(|inline| {
            inline
                .caller
                .arguments
                .as_ref()
                .map(|arguments| arguments.len())
        }) {
            None => self.add(Kind::LoadArgumentCount, &[], Repr::Int32),
            Some(Some(count)) => match i32::try_from(count) {
                Ok(count) => self.constant_int32(count),
                Err(_) => return self.generic(instruction),
            },
            Some(None) => return self.generic(instruction),
        };
        self.write(instruction.writes[0], count);
    }

    /// The elided `arguments[i]` for an int32 `i` naming an actual
    /// argument: one of a spliced call's argument values, or the physical
    /// activation's actual. Any other key, or an index past the actuals,
    /// leaves for the interpreter, whose ordinary `[[Get]]` materializes the
    /// object; a site that left once keeps the baseline operation.
    fn visit_arguments_element(&mut self, instruction: &Instruction) {
        if self.exited_before() {
            return self.generic(instruction);
        }
        let arguments = match self.inline.as_ref() {
            None => None,
            Some(inline) => match inline.caller.arguments.clone() {
                Some(arguments)
                    if !arguments.is_empty() && arguments.len() <= MAX_SELECTED_ARGUMENTS =>
                {
                    Some(arguments)
                }
                _ => return self.generic(instruction),
            },
        };
        let key = self.read(instruction.reads[0]);
        let index = self.int32(key);
        let value = match arguments {
            None => self.add(Kind::LoadActualArgument, &[index], Repr::Tagged),
            Some(arguments) => {
                if let Kind::ConstInt32(constant) = self.graph.node(index).kind {
                    match usize::try_from(constant)
                        .ok()
                        .and_then(|constant| arguments.get(constant))
                    {
                        Some(&argument) => self.tagged(argument),
                        None => return self.generic(instruction),
                    }
                } else {
                    let mut inputs: SmallVec<[NodeId; 4]> = smallvec::smallvec![index];
                    for &argument in &arguments {
                        let argument = self.tagged(argument);
                        inputs.push(argument);
                    }
                    let count = arguments.len() as u8;
                    self.add(Kind::SelectArgument { count }, &inputs, Repr::Tagged)
                }
            }
        };
        self.write(instruction.writes[0], value);
    }

    /// An element load that sees Array storage of several kinds or a name
    /// key, or whose speculation left for a bound, a hole or another layout:
    /// the generic keyed load, without a deopt.
    fn keyed_load(&mut self, instruction: &Instruction) {
        if !self.keyed_generic_site(instruction) {
            return self.generic(instruction);
        }
        let receiver = self.read(instruction.reads[0]);
        let receiver = self.tagged(receiver);
        let key = self.read(instruction.reads[1]);
        let key = self.tagged(key);
        let value = self.add(
            Kind::LoadKeyedCached { pc: self.pc },
            &[receiver, key],
            Repr::Tagged,
        );
        self.write(instruction.writes[0], value);
    }

    /// Whether the generic keyed operation serves this element site: one
    /// whose feedback names a typed array keeps its baseline operation, the
    /// keyed table routines need the cage, and the runtime completion names
    /// the physical frame's instruction, which an inlined body's is not.
    fn keyed_generic_site(&self, instruction: &Instruction) -> bool {
        let typed = matches!(
            self.view.element_accesses.get(&instruction.byte_pc),
            Some(access) if access.type_tag != otter_vm::array::ARRAY_BODY_TYPE_TAG
        );
        self.inline.is_none() && self.view.cage_base != 0 && !typed
    }

    /// Whether `value` is, or is speculated by this site's value feedback to
    /// be, an int32.
    fn int32_value(&self, value: NodeId) -> bool {
        let node = self.graph.node(value);
        match (&node.kind, node.repr) {
            (_, Repr::Int32) => return true,
            (_, Repr::Float64) => return false,
            (Kind::ConstTagged(bits), _) => {
                return otter_vm::Value::from_bits(*bits)
                    .as_number()
                    .and_then(|number| number.as_exact_i32())
                    .is_some();
            }
            _ => {}
        }
        if self
            .known
            .get(value)
            .is_some_and(|info| info.int32.is_some())
        {
            return true;
        }
        self.feedback().speculates_int32()
    }

    fn context_at_depth(&mut self, context: NodeId, depth: u16) -> NodeId {
        let mut context = context;
        for _ in 0..depth {
            let origin = self.closure_contexts.get(&context).copied();
            context = match self.known.parents.get(&context) {
                Some(&parent) => parent,
                None => {
                    let parent = self.add(Kind::LoadContextParent, &[context], Repr::Tagged);
                    self.known.parents.insert(context, parent);
                    parent
                }
            };
            if let Some((view, hops)) = origin {
                self.closure_contexts.insert(context, (view, hops + 1));
            }
        }
        context
    }

    /// The constant the body's baked closure fixes in `slot` of `context`,
    /// when `context` is that closure's context or an ancestor of it.
    fn closure_constant(
        &self,
        context: NodeId,
        slot: u16,
    ) -> Option<otter_vm::jit::JitContextConstant> {
        let &(view, depth) = self.closure_contexts.get(&context)?;
        if !std::ptr::eq(view, self.view) {
            return None;
        }
        self.view
            .closure_constants
            .iter()
            .find(|constant| constant.depth == depth && constant.slot == slot)
            .copied()
    }

    /// The node for a context slot's constant value, under a proof of the
    /// slot's dependency. An ordinary object of a stable shape carries that
    /// shape as a stale proof each use confirms.
    fn constant_slot_value(&mut self, constant: otter_vm::jit::JitContextConstant) -> NodeId {
        self.add(
            Kind::CheckContextSlot {
                watch: constant.watch,
                slot: constant.slot,
            },
            &[],
            Repr::None,
        );
        match constant.value {
            otter_vm::jit::JitConstantValue::Int32(value) => self.constant_int32(value),
            otter_vm::jit::JitConstantValue::Bits(bits) => {
                let value = self.constant_tagged(bits);
                self.known.entry(value).heap_object =
                    otter_vm::Value::from_bits(bits).is_object_type();
                value
            }
            otter_vm::jit::JitConstantValue::Cell {
                cell,
                object,
                shape,
                function,
                bag,
            } => {
                let value = self.add(Kind::LoadCellValue(cell), &[], Repr::Tagged);
                if let Some(function) = function {
                    self.constant_closures.insert(value, function);
                }
                if bag != 0 {
                    self.constant_bags.insert(value, bag);
                }
                let info = self.known.entry(value);
                info.heap_object = object;
                if shape != 0 {
                    debug_assert!(object);
                    info.ordinary_object = true;
                    self.known
                        .stale_shapes
                        .insert(value, smallvec::smallvec![shape]);
                }
                value
            }
        }
    }

    /// A context slot read; a `checked` read of a binding in its temporal
    /// dead zone leaves for the interpreter, which throws. A value already
    /// read or stored at the slot is that value, proved initialized unless
    /// it is the hole itself.
    fn visit_load_context_slot(&mut self, instruction: &Instruction, checked: bool) {
        let Some(coord) = instruction.imm32(2).and_then(ContextCoord::from_imm32) else {
            self.generic(instruction);
            return;
        };
        let context = self.read(instruction.reads[0]);
        let context = self.tagged(context);
        let context = self.context_at_depth(context, coord.depth);
        if let Some(constant) = self.closure_constant(context, coord.slot) {
            // A watched slot holds a value, never the hole.
            let value = self.constant_slot_value(constant);
            self.write(instruction.writes[0], value);
            return;
        }
        let offset = self.view.context_layout.slots_byte as i32 + i32::from(coord.slot) * 8;
        let key = FieldKey {
            object: context,
            property: false,
            offset,
        };
        if let Some(&value) = self.known.fields.get(&key) {
            if checked
                && matches!(self.graph.node(value).kind, Kind::ConstTagged(bits) if bits == tag::VALUE_HOLE)
            {
                self.add(Kind::CheckNotHole, &[value], Repr::None);
            }
            self.write(instruction.writes[0], value);
            return;
        }
        let value = self.add(Kind::LoadTaggedField(offset), &[context], Repr::Tagged);
        if checked {
            self.add(Kind::CheckNotHole, &[value], Repr::None);
        }
        self.known.fields.insert(key, value);
        self.write(instruction.writes[0], value);
    }

    fn visit_store_context_slot(&mut self, instruction: &Instruction) {
        let Some(coord) = instruction.imm32(2).and_then(ContextCoord::from_imm32) else {
            self.generic(instruction);
            return;
        };
        let value = self.read(instruction.reads[0]);
        let value = self.tagged(value);
        let context = self.read(instruction.reads[1]);
        let context = self.tagged(context);
        let context = self.context_at_depth(context, coord.depth);
        let offset = self.view.context_layout.slots_byte as i32 + i32::from(coord.slot) * 8;
        self.add(
            Kind::StoreTaggedField(offset),
            &[context, value],
            Repr::None,
        );
        self.write_barrier(context, value);
        if coord.slot < otter_vm::code_dependencies::WATCHABLE_CONTEXT_SLOTS {
            self.add(Kind::NoteContextWrite(coord.slot), &[context], Repr::None);
        }
        self.known.store_field(
            FieldKey {
                object: context,
                property: false,
                offset,
            },
            value,
        );
    }
}

/// The only target Graph can call, retaining its canonical vector index.
/// Template-only bodies do not create Graph call or inline admission.
fn single_method_target(
    methods: &[otter_vm::jit::JitMethodTarget],
) -> Option<(
    u8,
    &otter_vm::jit::JitMethodTarget,
    &otter_vm::jit::JitDirectCallee,
)> {
    let mut targets = methods
        .iter()
        .enumerate()
        .filter_map(|(index, method)| Some((index, method, method.callee.as_ref()?)));
    let (index, method, callee) = targets.next()?;
    if targets.next().is_some() {
        return None;
    }
    Some((u8::try_from(index).ok()?, method, callee))
}

/// Whether the guarded load of `method` reads its code-retained method cell
/// rather than the holder's slot.
fn reads_method_cell(method: &otter_vm::jit::JitMethodTarget) -> bool {
    method.guard.method_cell != 0 && method.guard.accessor_byte.is_none()
}
