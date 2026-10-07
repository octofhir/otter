//! Element storage addresses stay within their noncollecting graph segment.
//!
//! # Contents
//! - Boxed and numeric array loads around collecting nodes and branch joins.
//! - Loop-header and OSR invalidation, including LICM and backedge liveness.
//! - Emitted boxed-hole guards with exact pre-load eager recovery.
//!
//! # Invariants
//! These fixtures build production graphs without executing their geometry.
//! An element base is an untraced interior pointer; its tagged owner stays live
//! across collection, and each later access reads a fresh base from that owner.
//! Loop and OSR headers conservatively discard incoming storage facts; the
//! current native backedge poll is a no-allocation leaf.
//!
//! # See also
//! - `otter-runtime/tests/jit_empty_allocations.rs` executes moving young slabs.

use std::rc::Rc;

use otter_bytecode::{Op, Operand};
use otter_vm::{
    JitCompileSnapshot,
    jit::{JitElementAccess, JitElementRepr},
};

use super::{graph, snapshot};
use crate::graph::{
    BaselineSupport,
    builder::{Built, build},
    bytecode::Analysis,
    ir::{Kind, NodeId, Repr},
    licm,
    liveness::Liveness,
};

fn load(destination: u16) -> (Op, Vec<Operand>) {
    (
        Op::LoadElement,
        vec![
            Operand::Register(destination),
            Operand::Register(0),
            Operand::Register(1),
        ],
    )
}

fn store() -> (Op, Vec<Operand>) {
    (
        Op::StoreElement,
        vec![
            Operand::Register(0),
            Operand::Register(1),
            Operand::Register(4),
        ],
    )
}

fn seed_elements(view: &mut JitCompileSnapshot, element: JitElementRepr, pcs: &[u32]) {
    // Production requires an active cage before accepting element geometry.
    // This aligned address is passive fixture data: these graphs never emit
    // or execute a dereference through it.
    view.cage_base = 0x1_0000_0000;
    let mut access = JitElementAccess::packed_double_array();
    access.element = element;
    if element == JitElementRepr::Boxed {
        // The fixture never executes its guard; only its InBody storage and
        // boxed representation feed construction. Do not invent a VM-private
        // tagged-kind discriminant in the JIT tests.
        access.guards[1] = None;
    }
    for &pc in pcs {
        let instruction = &view.instructions[pc as usize];
        assert!(matches!(
            instruction.op(&view.code_block),
            Op::LoadElement | Op::StoreElement
        ));
        // Seed the byte position owned by the fixture's authoritative
        // CodeBlock; the helper preserves its supplied diagnostic positions.
        view.element_accesses.insert(instruction.byte_pc, access);
    }
}

fn bases(built: &Built) -> Vec<NodeId> {
    built
        .graph
        .nodes
        .iter()
        .enumerate()
        .filter_map(|(index, node)| {
            matches!(node.kind, Kind::LoadElementsBase { .. }).then_some(NodeId(index as u32))
        })
        .collect()
}

fn assert_no_base_crosses(built: &Built, boundary: NodeId) {
    let live = Liveness::compute(&built.graph, &built.layout);
    for base in bases(built) {
        assert_eq!(built.graph.node(base).repr, Repr::Word);
        assert!(
            !live.is_live_after(boundary, base),
            "untraced base {base:?} is live across {:?}",
            built.graph.node(boundary).kind
        );
    }
}

fn assert_refreshed_accesses(built: &Built, before_pc: u32, after_pc: u32) -> [NodeId; 2] {
    let load_at = |pc| {
        built
            .graph
            .nodes
            .iter()
            .enumerate()
            .find(|(_, node)| node.pc == pc && matches!(node.kind, Kind::LoadElement(_)))
            .map(|(index, _)| NodeId(index as u32))
            .expect("own specialized array load")
    };
    let before = load_at(before_pc);
    let after = load_at(after_pc);
    let before_base = built.graph.node(before).inputs[0];
    let after_base = built.graph.node(after).inputs[0];
    assert_ne!(before, after, "a collecting path cannot common the load");
    assert_ne!(before_base, after_base, "read the relocated slab base");
    let owner = built.graph.node(before_base).inputs[0];
    assert_eq!(built.graph.node(after_base).inputs[0], owner);
    assert_eq!(built.graph.node(owner).repr, Repr::Tagged);
    let stored = built
        .graph
        .nodes
        .iter()
        .find(|node| matches!(node.kind, Kind::StoreElement(_)))
        .expect("own post-boundary store");
    assert_eq!(stored.inputs[0], after_base);
    [before_base, after_base]
}

#[test]
fn boxed_load_proves_own_presence_before_native_read_and_recovers_its_exact_pc() {
    let mut view = snapshot(
        90,
        2,
        5,
        vec![load(4), (Op::ReturnValue, vec![Operand::Register(4)])],
    );
    seed_elements(&mut view, JitElementRepr::Boxed, &[0]);
    let transitions = crate::entry::TransitionTable::resolve();
    let compiled = crate::graph::compile(&view, 7007, &transitions, None, false).unwrap();
    let graph = &compiled.built.graph;
    let (guard_id, guard) = graph
        .nodes
        .iter()
        .enumerate()
        .find(|(_, node)| node.kind == Kind::CheckElementPresent)
        .map(|(index, node)| (NodeId(index as u32), node))
        .expect("prototype-only admission requires an own-present proof");
    let (load_id, load) = graph
        .nodes
        .iter()
        .enumerate()
        .find(|(_, node)| node.kind == Kind::LoadElement(JitElementRepr::Boxed))
        .map(|(index, node)| (NodeId(index as u32), node))
        .expect("own specialized boxed read");
    assert_eq!(guard.inputs, load.inputs);
    assert_eq!(guard.pc, 0);
    assert!(guard.eager.is_some() && guard.lazy.is_none());
    assert!(!guard.kind.properties().may_collect);
    let offsets = &compiled.emission.node_offsets;
    let guard_position = offsets.iter().position(|(_, id)| *id == guard_id).unwrap();
    let load_position = offsets.iter().position(|(_, id)| *id == load_id).unwrap();
    assert!(
        guard_position < load_position,
        "guard executes before the value read"
    );
    assert!(
        offsets[guard_position + 1].0 > offsets[guard_position].0,
        "the own-presence check emits real native instructions"
    );
    let exit_index = compiled
        .emission
        .exits
        .iter()
        .position(|site| {
            site.node == guard_id
                && !site.lazy
                && site.reason == otter_vm::native_abi::ExitReason::BoundsGuard
        })
        .expect("a hole leaves through the pre-read eager exit");
    assert_eq!(compiled.deopt.exits[exit_index].resume_pcs.as_ref(), &[0]);
    assert_eq!(
        compiled.deopt.exits[exit_index].action,
        otter_vm::native_abi::ExitAction::Recompile
    );
}

#[test]
fn boxed_and_numeric_bases_expire_at_every_collecting_kind() {
    let boundaries = [
        (Op::NewObject, vec![Operand::Register(5)], Kind::NewObject),
        (
            Op::NewArray,
            vec![Operand::Register(5), Operand::ConstIndex(0)],
            Kind::NewArrayEmpty,
        ),
        (
            Op::NewArray,
            vec![
                Operand::Register(5),
                Operand::ConstIndex(1),
                Operand::Register(2),
            ],
            Kind::NewArrayLiteral,
        ),
        (
            Op::Instanceof,
            vec![
                Operand::Register(5),
                Operand::Register(0),
                Operand::Register(2),
            ],
            Kind::Instanceof,
        ),
        (
            Op::ArrayPush,
            vec![Operand::Register(0), Operand::Register(2)],
            Kind::Generic {
                pc: 1,
                registers: vec![0, 2].into_boxed_slice(),
            },
        ),
    ];
    for element in [JitElementRepr::Boxed, JitElementRepr::Float64] {
        for (op, operands, expected) in &boundaries {
            let mut view = snapshot(
                90,
                3,
                7,
                vec![
                    load(4),
                    (*op, operands.clone()),
                    load(6),
                    store(),
                    (Op::ReturnValue, vec![Operand::Register(6)]),
                ],
            );
            seed_elements(&mut view, element, &[0, 2, 3]);
            let built = graph(&view);
            let boundary = built
                .graph
                .nodes
                .iter()
                .position(|node| node.kind == *expected)
                .map(|index| NodeId(index as u32))
                .expect("exact collecting kind");
            let properties = expected.properties();
            assert!(properties.call || properties.may_collect);
            let [before, after] = assert_refreshed_accesses(&built, 0, 2);
            assert_eq!(bases(&built), [before, after]);
            assert_no_base_crosses(&built, boundary);
            let owner = built.graph.node(before).inputs[0];
            assert!(
                Liveness::compute(&built.graph, &built.layout).is_live_after(boundary, owner),
                "the collector must retain the array owner"
            );
        }
    }
}

#[test]
fn a_collecting_branch_invalidates_the_base_at_the_join() {
    for element in [JitElementRepr::Boxed, JitElementRepr::Float64] {
        let mut view = snapshot(
            90,
            4,
            7,
            vec![
                load(4),
                (
                    Op::JumpIfFalse,
                    vec![Operand::Imm32(2), Operand::Register(3)],
                ),
                (Op::NewObject, vec![Operand::Register(5)]),
                (Op::Jump, vec![Operand::Imm32(0)]),
                load(6),
                store(),
                (Op::ReturnValue, vec![Operand::Register(6)]),
            ],
        );
        seed_elements(&mut view, element, &[0, 4, 5]);
        let built = graph(&view);
        let [before, after] = assert_refreshed_accesses(&built, 0, 4);
        assert_eq!(bases(&built), [before, after]);
        let join = built.graph.node(after).block.unwrap();
        assert_eq!(built.graph.block(join).predecessors.len(), 2);
        let boundary = built
            .graph
            .nodes
            .iter()
            .position(|node| node.kind == Kind::NewObject)
            .map(|index| NodeId(index as u32))
            .unwrap();
        assert_no_base_crosses(&built, boundary);
    }
}

#[test]
fn loop_and_osr_headers_reload_bases_that_licm_cannot_hoist() {
    for element in [JitElementRepr::Boxed, JitElementRepr::Float64] {
        for osr_pc in [None, Some(1)] {
            let mut view = snapshot(
                90,
                3,
                6,
                vec![
                    load(4),
                    load(5),
                    store(),
                    (
                        Op::JumpIfTrue,
                        vec![Operand::Imm32(-3), Operand::Register(2)],
                    ),
                    (Op::ReturnValue, vec![Operand::Register(5)]),
                ],
            );
            seed_elements(&mut view, element, &[0, 1, 2]);
            let analysis = Rc::new(Analysis::build(&view).unwrap());
            let mut built = build(
                &view,
                &analysis,
                &BaselineSupport {
                    supported: vec![true; view.instructions.len()],
                },
                osr_pc,
            )
            .unwrap();
            assert_eq!(built.osr_entry.is_some(), osr_pc.is_some());
            assert_eq!(built.loop_headers.len(), 1);
            let header = built.loop_headers[0].block;
            let old_bases = bases(&built);
            assert_eq!(old_bases.len(), 2, "the header forgets pre-loop storage");
            let header_base = old_bases
                .iter()
                .copied()
                .find(|&base| built.graph.node(base).pc == 1)
                .expect("own header base");
            assert_eq!(built.graph.node(header_base).block, Some(header));
            licm::hoist_invariants(&mut built.graph, &mut built.layout, &built.loop_headers);
            assert_eq!(bases(&built), old_bases);
            assert_eq!(
                built.graph.node(header_base).block,
                Some(header),
                "LICM keeps the loop-header base load inside the loop"
            );
            let polls: Vec<_> = built
                .graph
                .nodes
                .iter()
                .enumerate()
                .filter_map(|(index, node)| {
                    matches!(node.kind, Kind::JumpLoop(_)).then_some(NodeId(index as u32))
                })
                .collect();
            assert_eq!(polls.len(), 1);
            assert_no_base_crosses(&built, polls[0]);
        }
    }
}
