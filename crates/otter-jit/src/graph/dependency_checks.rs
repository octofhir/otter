//! Monotone facts as code dependencies: one flag test per effect region.
//!
//! The builder places a stability, field-summary or context-slot proof
//! before every use of such a fact, as if each re-read its word after any
//! operation that may have run code, and an inherited method load names the
//! prototype-chain validity cell it relies on. The generation instead
//! subscribes to those facts at installation, and a writer that breaks one
//! sets its dependency flag. This pass collects the facts as the
//! generation's dependencies and keeps one flag test where a fact is first
//! relied on since entry or since a node that can break one; every other
//! proof goes, and method loads read no validity cell.
//!
//! # Contents
//! - [`place`] — rewrite proofs into flag tests and return the dependencies.
//!
//! # Invariants
//! - A fact can break only through a node that runs code or collects, a named
//!   store (its transitions leave shapes and observe field summaries), an
//!   own-field store that observes a summary, or a context-slot store whose
//!   watch may fire, or a closure birth that can break source-context identity.
//!   Everything else, including an interrupt poll, leaves
//!   every fact as it was.
//! - A block entered from outside the graph (function or OSR entry) or with
//!   no predecessor starts unchecked; a join is checked only when every
//!   predecessor leaves checked.
//! - Each removed proof is dominated, within an effect-free stretch, by a
//!   kept test, and keeps no frame state. A method load reached unchecked
//!   gets a test of its own before it, leaving through its eager state.
//!
//! # See also
//! - [`otter_vm::code_dependencies`] for subscription and firing.
//! - [`super::builder_field_type`] for where proofs are placed.

use std::collections::BTreeSet;
use std::sync::Arc;

use otter_vm::JitCompileSnapshot;
use otter_vm::native_abi::{CodeDependency, CodeDependencyKind};

use super::ir::{Graph, Kind, NodeId, Repr};

/// Whether `node` can break a subscribed shape fact.
fn breaks_facts(graph: &Graph, node: NodeId) -> bool {
    let kind = &graph.node(node).kind;
    let properties = kind.properties();
    if properties.call || properties.may_collect {
        return true;
    }
    match kind {
        Kind::StoreNamedProperty { .. } | Kind::NoteContextWrite(_) => true,
        Kind::StoreOwnField { owners, .. } => !owners.is_empty(),
        _ => false,
    }
}

fn is_proof(kind: &Kind) -> bool {
    matches!(
        kind,
        Kind::CheckStableShapes(_)
            | Kind::CheckFieldTypes(_)
            | Kind::CheckContextSlot { .. }
            | Kind::CheckClosureContext(_)
    )
}

/// The validity cell an inherited method load relies on.
fn method_validity(
    view: &JitCompileSnapshot,
    inline_views: &[Arc<JitCompileSnapshot>],
    graph: &Graph,
    node: NodeId,
) -> Option<usize> {
    let Kind::LoadGuardedMethod {
        byte_pc, target, ..
    } = graph.node(node).kind
    else {
        return None;
    };
    super::metadata::source_view(view, inline_views, graph, node)
        .direct_methods
        .get(&byte_pc)?
        .get(usize::from(target))?
        .guard
        .prototype_validity
        .map(|validity| validity.address)
}

/// Rewrite the graph's stability and field-summary proofs into dependency
/// flag tests, test the flag before method loads that rely on a validity
/// cell, and return the facts, in a deterministic order.
pub(crate) fn place(
    graph: &mut Graph,
    view: &JitCompileSnapshot,
    inline_views: &[Arc<JitCompileSnapshot>],
) -> Box<[CodeDependency]> {
    let mut facts = BTreeSet::new();
    let mut methods = rustc_hash::FxHashSet::default();
    for block in &graph.blocks {
        for &node in &block.body {
            if let Some(address) = method_validity(view, inline_views, graph, node) {
                methods.insert(node);
                facts.insert((
                    CodeDependencyKind::PrototypeValidity as u16,
                    0,
                    address as u64,
                ));
                continue;
            }
            match &graph.node(node).kind {
                Kind::CheckStableShapes(shapes) => {
                    facts.extend(
                        shapes
                            .iter()
                            .map(|&shape| (CodeDependencyKind::ShapeStable as u16, shape, 1)),
                    );
                }
                Kind::CheckFieldTypes(guards) => {
                    facts.extend(guards.iter().map(|guard| {
                        (
                            CodeDependencyKind::FieldType as u16,
                            guard.owner_shape,
                            u64::from(guard.expected as u8),
                        )
                    }));
                }
                &Kind::CheckClosureContext(function_id) => {
                    facts.insert((CodeDependencyKind::ClosureContext as u16, function_id, 0));
                }
                &Kind::CheckContextSlot { watch, slot } => {
                    facts.insert((
                        CodeDependencyKind::ContextSlot as u16,
                        watch,
                        u64::from(slot),
                    ));
                }
                _ => {}
            }
        }
    }
    if facts.is_empty() {
        return Box::new([]);
    }
    // Checked-on-exit per block, optimistic for every block but the entries,
    // lowered until no block changes.
    let count = graph.blocks.len();
    let mut checked_out = vec![true; count];
    let entry_state = |graph: &Graph, checked_out: &[bool], block: usize| -> bool {
        let predecessors = &graph.blocks[block].predecessors;
        !predecessors.is_empty()
            && predecessors
                .iter()
                .all(|predecessor| checked_out[predecessor.0 as usize])
    };
    let transfer = |graph: &Graph, block: usize, mut checked: bool| -> bool {
        for &node in &graph.blocks[block].body {
            if is_proof(&graph.node(node).kind) || methods.contains(&node) {
                checked = true;
            } else if breaks_facts(graph, node) {
                checked = false;
            }
        }
        checked
    };
    loop {
        let mut changed = false;
        for block in 0..count {
            let out = transfer(graph, block, entry_state(graph, &checked_out, block));
            if out != checked_out[block] {
                checked_out[block] = out;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    let mut tests = Vec::new();
    let mut removed = Vec::new();
    let mut guarded_loads = Vec::new();
    for block in 0..count {
        let mut checked = entry_state(graph, &checked_out, block);
        for &node in &graph.blocks[block].body {
            if is_proof(&graph.node(node).kind) {
                if checked {
                    removed.push(node);
                } else {
                    tests.push(node);
                    checked = true;
                }
            } else if methods.contains(&node) {
                if !checked {
                    guarded_loads.push((block, node));
                    checked = true;
                }
            } else if breaks_facts(graph, node) {
                checked = false;
            }
        }
    }
    for node in tests {
        graph.node_mut(node).kind = Kind::CheckDependencies;
    }
    for (block, load) in guarded_loads {
        let (pc, origin, eager) = {
            let data = graph.node(load);
            (data.pc, data.origin, data.eager)
        };
        let test = graph.add_node(Kind::CheckDependencies, &[], Repr::None);
        let data = graph.node_mut(test);
        data.pc = pc;
        data.origin = origin;
        data.eager = eager;
        data.block = Some(super::ir::BlockId(block as u32));
        let body = &mut graph.blocks[block].body;
        let index = body
            .iter()
            .position(|&node| node == load)
            .expect("a placed method load");
        body.insert(index, test);
    }
    let removed: rustc_hash::FxHashSet<NodeId> = removed.into_iter().collect();
    for &node in &removed {
        let data = graph.node_mut(node);
        data.block = None;
        data.eager = None;
    }
    for block in &mut graph.blocks {
        block.body.retain(|node| !removed.contains(node));
    }
    facts
        .into_iter()
        .map(|(kind, identity, expected)| CodeDependency {
            kind: match kind {
                kind if kind == CodeDependencyKind::ShapeStable as u16 => {
                    CodeDependencyKind::ShapeStable
                }
                kind if kind == CodeDependencyKind::FieldType as u16 => {
                    CodeDependencyKind::FieldType
                }
                kind if kind == CodeDependencyKind::ContextSlot as u16 => {
                    CodeDependencyKind::ContextSlot
                }
                kind if kind == CodeDependencyKind::ClosureContext as u16 => {
                    CodeDependencyKind::ClosureContext
                }
                _ => CodeDependencyKind::PrototypeValidity,
            },
            identity,
            expected,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::field_store::FieldStoreOwner;
    use crate::graph::ir::{BlockId, FrameState};
    use otter_vm::jit::JitFieldTypeGuard;
    use smallvec::smallvec;

    fn append(graph: &mut Graph, block: BlockId, kind: Kind) -> NodeId {
        let node = graph.add_node(kind, &[], Repr::None);
        let state = graph.add_frame_state(FrameState {
            function_id: 1,
            pc: 0,
            byte_pc: 0,
            register_count: 0,
            registers: Vec::new(),
            caller: None,
        });
        let data = graph.node_mut(node);
        data.block = Some(block);
        data.eager = Some(state);
        graph.block_mut(block).body.push(node);
        node
    }

    fn guard(owner_shape: u32, expected: u64) -> JitFieldTypeGuard {
        JitFieldTypeGuard {
            owner_shape,
            expected,
        }
    }

    fn generic() -> Kind {
        Kind::Generic {
            pc: 0,
            registers: Box::new([]),
        }
    }

    fn observing_store() -> Kind {
        Kind::StoreOwnField {
            field: otter_vm::object::FieldLocation::inline(0),
            owners: smallvec![(
                40,
                FieldStoreOwner {
                    owner_shape: 48,
                    stored_shapes: SmallVec::new(),
                    nullish_mask: 0,
                    int32: false,
                    int32_constant: None,
                }
            )],
        }
    }

    fn kinds(graph: &Graph, block: BlockId) -> Vec<&Kind> {
        graph
            .block(block)
            .body
            .iter()
            .map(|&node| &graph.node(node).kind)
            .collect()
    }

    use smallvec::SmallVec;

    fn view() -> JitCompileSnapshot {
        JitCompileSnapshot::without_feedback(0, 0, 0, vec![])
    }

    #[test]
    fn one_test_per_effect_region_and_the_facts_are_the_dependencies() {
        let mut graph = Graph::default();
        let entry = graph.new_block();
        append(&mut graph, entry, Kind::CheckStableShapes(smallvec![8]));
        append(
            &mut graph,
            entry,
            Kind::CheckFieldTypes(smallvec![guard(16, 0x184)]),
        );
        append(&mut graph, entry, generic());
        append(&mut graph, entry, Kind::CheckStableShapes(smallvec![8]));
        append(&mut graph, entry, Kind::CheckStableShapes(smallvec![24]));
        let dependencies = place(&mut graph, &view(), &[]);
        assert_eq!(
            kinds(&graph, entry),
            [
                &Kind::CheckDependencies,
                &generic(),
                &Kind::CheckDependencies
            ]
        );
        assert_eq!(
            dependencies
                .iter()
                .map(|dependency| (dependency.kind, dependency.identity, dependency.expected))
                .collect::<Vec<_>>(),
            [
                (CodeDependencyKind::ShapeStable, 8, 1),
                (CodeDependencyKind::ShapeStable, 24, 1),
                (CodeDependencyKind::FieldType, 16, 0x84),
            ]
        );
    }

    #[test]
    fn joins_and_back_edges_stay_checked_only_without_breaking_nodes() {
        // entry -> header <-> body; the body observes a field summary.
        let mut graph = Graph::default();
        let entry = graph.new_block();
        let header = graph.new_block();
        let body = graph.new_block();
        let quiet = graph.new_block();
        append(&mut graph, entry, Kind::CheckStableShapes(smallvec![8]));
        graph.block_mut(header).predecessors = vec![entry, body];
        graph.block_mut(header).is_loop = true;
        append(&mut graph, header, Kind::CheckStableShapes(smallvec![8]));
        graph.block_mut(body).predecessors = vec![header];
        append(&mut graph, body, observing_store());
        // A second loop whose body breaks nothing keeps its entry's test.
        graph.block_mut(quiet).predecessors = vec![body, quiet];
        append(&mut graph, quiet, Kind::CheckStableShapes(smallvec![8]));
        place(&mut graph, &view(), &[]);
        assert_eq!(kinds(&graph, entry), [&Kind::CheckDependencies]);
        assert_eq!(kinds(&graph, header), [&Kind::CheckDependencies]);
        assert_eq!(kinds(&graph, quiet), [&Kind::CheckDependencies]);
        let mut graph = Graph::default();
        let entry = graph.new_block();
        let header = graph.new_block();
        append(&mut graph, entry, Kind::CheckStableShapes(smallvec![8]));
        graph.block_mut(header).predecessors = vec![entry, header];
        append(
            &mut graph,
            header,
            Kind::CheckFieldTypes(smallvec![guard(16, 4)]),
        );
        place(&mut graph, &view(), &[]);
        assert!(kinds(&graph, header).is_empty());
    }

    #[test]
    fn a_context_slot_store_reopens_the_region_of_a_slot_constant() {
        let mut graph = Graph::default();
        let entry = graph.new_block();
        let constant = Kind::CheckContextSlot { watch: 5, slot: 2 };
        append(&mut graph, entry, constant.clone());
        append(
            &mut graph,
            entry,
            Kind::CheckContextSlot { watch: 5, slot: 3 },
        );
        append(&mut graph, entry, Kind::NoteContextWrite(2));
        append(&mut graph, entry, constant);
        let dependencies = place(&mut graph, &view(), &[]);
        assert_eq!(
            kinds(&graph, entry),
            [
                &Kind::CheckDependencies,
                &Kind::NoteContextWrite(2),
                &Kind::CheckDependencies
            ]
        );
        assert_eq!(
            dependencies
                .iter()
                .map(|dependency| (dependency.kind, dependency.identity, dependency.expected))
                .collect::<Vec<_>>(),
            [
                (CodeDependencyKind::ContextSlot, 5, 2),
                (CodeDependencyKind::ContextSlot, 5, 3),
            ]
        );
    }

    #[test]
    fn a_closure_birth_reopens_the_source_context_fact_region() {
        let mut graph = Graph::default();
        let entry = graph.new_block();
        append(&mut graph, entry, Kind::CheckClosureContext(7));
        append(
            &mut graph,
            entry,
            Kind::CheckContextSlot { watch: 5, slot: 2 },
        );
        append(&mut graph, entry, Kind::NewClosure);
        append(&mut graph, entry, Kind::CheckClosureContext(7));
        let dependencies = place(&mut graph, &view(), &[]);
        assert_eq!(
            kinds(&graph, entry),
            [
                &Kind::CheckDependencies,
                &Kind::NewClosure,
                &Kind::CheckDependencies,
            ]
        );
        assert!(dependencies.contains(&CodeDependency {
            kind: CodeDependencyKind::ClosureContext,
            identity: 7,
            expected: 0,
        }));
    }

    #[test]
    fn a_graph_without_facts_keeps_its_nodes() {
        let mut graph = Graph::default();
        let entry = graph.new_block();
        append(&mut graph, entry, generic());
        assert!(place(&mut graph, &view(), &[]).is_empty());
        assert_eq!(kinds(&graph, entry), [&generic()]);
    }
}
