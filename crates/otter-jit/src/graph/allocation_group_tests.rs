//! Fixed-group original identity, effect barriers and emitted recovery proofs.
//!
//! # Contents
//! - Bounded post-LICM groups with tagged projections and exact source lineage.
//! - Intervening observable operations/guards/blocks retain separate allocations.
//! - Current emitted group admission has the FIRST source eager resume and roots.
//!
//! # Invariants
//! Passive fixture plans never reach the collector. The actual pipeline emits
//! target code, and every source allocation keeps its original node id/PC.
//! Projection cells are tagged and pure; no Word interior SSA survives the group.
//!
//! # See also
//! - `allocation::string_group_tests` executes both native complete writers.
//! - Runtime group failure tests exercise actual first-source recovery.

use super::{graph, snapshot};
use crate::graph::{
    allocation_groups,
    ir::{Kind, Repr},
    licm,
};
use otter_bytecode::{Op, Operand};
use otter_vm::jit::JitEmptyObjectAllocationPlan;

fn view(code: Vec<(Op, Vec<Operand>)>) -> otter_vm::JitCompileSnapshot {
    let mut v = snapshot(90, 1, 12, code);
    v.literal_allocations.object = Some(JitEmptyObjectAllocationPlan::new(4096));
    v.literal_allocations.group_allowed = true;
    v
}
#[test]
fn consecutive_shells_keep_original_ids_first_eager_state_and_tagged_projections() {
    let mut code = Vec::new();
    for index in 1..=9 {
        code.push((Op::NewObject, vec![Operand::Register(index)]));
    }
    code.push((Op::ReturnValue, vec![Operand::Register(9)]));
    let v = view(code);
    let mut built = graph(&v);
    let ids: Vec<_> = built
        .graph
        .nodes
        .iter()
        .enumerate()
        .filter(|(_, n)| n.kind == Kind::NewObject)
        .map(|(index, n)| {
            (
                crate::graph::ir::NodeId(index as u32),
                n.pc,
                n.origin,
                n.eager,
            )
        })
        .collect();
    assert_eq!(ids.len(), 9);
    licm::hoist_invariants(&mut built.graph, &mut built.layout, &built.loop_headers);
    allocation_groups::fold(&mut built.graph, &built.layout, &v, &built.inline_views);
    assert_eq!(built.graph.allocation_groups.len(), 1);
    let group = &built.graph.allocation_groups[0];
    assert_eq!(group.members.len(), 8);
    assert_eq!(group.bytes, 8 * group.members[0].layout.bytes());
    for (index, member) in group.members.iter().enumerate() {
        assert_eq!(member.node, ids[index].0);
        let node = built.graph.node(member.node);
        assert_eq!((node.pc, node.origin), (ids[index].1, ids[index].2));
        assert_eq!(node.repr, Repr::Tagged);
        if index == 0 {
            assert_eq!(node.kind, Kind::AllocationGroup(0));
            assert_eq!(node.eager, ids[0].3);
        } else {
            assert_eq!(node.kind, Kind::AllocationProjection(member.byte));
            assert_eq!(&node.inputs[..], &[ids[0].0]);
            assert!(node.eager.is_none());
            assert!(!node.kind.properties().may_collect && !node.kind.properties().effectful);
        }
    }
    assert_eq!(
        built.graph.node(ids[8].0).kind,
        Kind::NewObject,
        "8-member bound retains the ninth source operation"
    );
    let first = ids[0].0;
    let eager = built.graph.node(first).eager.unwrap();
    assert_eq!(
        built.graph.frame_state(eager).pc,
        ids[0].1,
        "refusal resumes before the first allocation"
    );
}
#[test]
fn an_intervening_source_effect_cannot_fold_its_surrounding_allocations() {
    let v = view(vec![
        (Op::NewObject, vec![Operand::Register(1)]),
        (
            Op::ArrayPush,
            vec![Operand::Register(0), Operand::Register(1)],
        ),
        (Op::NewObject, vec![Operand::Register(3)]),
        (Op::ReturnValue, vec![Operand::Register(3)]),
    ]);
    let mut built = graph(&v);
    allocation_groups::fold(&mut built.graph, &built.layout, &v, &built.inline_views);
    assert!(built.graph.allocation_groups.is_empty());
    assert_eq!(
        built
            .graph
            .nodes
            .iter()
            .filter(|n| n.kind == Kind::NewObject)
            .count(),
        2
    );
}
#[test]
fn actual_emission_records_one_group_probe_and_exact_first_source_eager_recovery() {
    let v = view(vec![
        (Op::NewObject, vec![Operand::Register(1)]),
        (Op::NewObject, vec![Operand::Register(2)]),
        (Op::ReturnValue, vec![Operand::Register(1)]),
    ]);
    let transitions = crate::entry::TransitionTable::resolve();
    let compiled = crate::graph::compile(&v, 9191, &transitions, None, false)
        .expect("current group target emission");
    assert_eq!(compiled.built.graph.allocation_groups.len(), 1);
    let graph = &compiled.built.graph;
    let first = graph.allocation_groups[0].members[0].node;
    let node = graph.node(first);
    assert_eq!(node.pc, 0);
    assert_eq!(graph.frame_state(node.eager.unwrap()).pc, 0);
    assert!(!compiled.emission.buffer.is_empty());
    assert!(
        compiled.emission.site_records.iter().any(|record| record.id
            == crate::graph::metadata::FIRST_SITE_SAFEPOINT
            && !record.spill_roots.is_empty()),
        "the group uses the real initialized canonical root record"
    );
    let exits: Vec<_> = compiled
        .emission
        .exits
        .iter()
        .enumerate()
        .filter(|(_, site)| site.node == first)
        .collect();
    assert!(!exits.is_empty(), "actual admission has eager status exits");
    for (index, site) in exits {
        assert!(!site.lazy);
        assert_eq!(
            compiled.deopt.exits[index].resume_pcs.as_ref(),
            &[0],
            "real emitted eager exit resumes FIRST source allocation"
        );
    }
}

#[test]
fn disabled_and_unprepared_source_snapshots_keep_standalone_native_allocations() {
    assert!(!otter_vm::jit::JitLiteralAllocationPlans::default().group_allowed);
    let mut v = view(vec![
        (Op::NewObject, vec![Operand::Register(1)]),
        (Op::NewObject, vec![Operand::Register(2)]),
        (Op::ReturnValue, vec![Operand::Register(1)]),
    ]);
    v.literal_allocations.group_allowed = false;
    let transitions = crate::entry::TransitionTable::resolve();
    let compiled = crate::graph::compile(&v, 9192, &transitions, None, false)
        .expect("disabled policy still compiles standalone native shells");
    assert!(compiled.built.graph.allocation_groups.is_empty());
    assert_eq!(
        compiled
            .built
            .graph
            .nodes
            .iter()
            .filter(|n| n.kind == Kind::NewObject)
            .count(),
        2
    );
    assert!(
        !compiled
            .built
            .graph
            .nodes
            .iter()
            .any(|n| matches!(n.kind, Kind::AllocationProjection(_)))
    );
    assert!(!compiled.emission.buffer.is_empty());
}

#[test]
fn grouping_uses_the_exact_inline_source_capability_instead_of_the_root_capability() {
    let mut root = view(vec![
        (Op::NewObject, vec![Operand::Register(1)]),
        (Op::NewObject, vec![Operand::Register(2)]),
        (Op::ReturnValue, vec![Operand::Register(1)]),
    ]);
    let mut inline = root.clone();
    inline.literal_allocations.group_allowed = false;
    let mut built = graph(&root);
    for node in &mut built.graph.nodes {
        if node.kind == Kind::NewObject {
            node.origin = 1;
        }
    }
    allocation_groups::fold(
        &mut built.graph,
        &built.layout,
        &root,
        &[std::sync::Arc::new(inline.clone())],
    );
    assert!(
        built.graph.allocation_groups.is_empty(),
        "root admission cannot admit a disabled innermost source"
    );
    assert_eq!(
        built
            .graph
            .nodes
            .iter()
            .filter(|n| n.kind == Kind::NewObject)
            .count(),
        2
    );
    root.literal_allocations.group_allowed = false;
    inline.literal_allocations.group_allowed = true;
    allocation_groups::fold(
        &mut built.graph,
        &built.layout,
        &root,
        &[std::sync::Arc::new(inline)],
    );
    assert_eq!(
        built.graph.allocation_groups.len(),
        1,
        "an enabled exact source does not inherit root refusal"
    );
    for member in &built.graph.allocation_groups[0].members {
        assert_eq!(built.graph.node(member.node).origin, 1);
    }
}
