//! Canonical-home and control-flow-liveness regressions.
//!
//! # Contents
//! - Collecting slow paths, representation separation and interval reuse.
//! - Path-dead values at an inner-loop entry and constant rematerialization.
//! - GP and FP exhaustion across collecting operations, calls and backedges.
//! - Dead-home expiration preserving memory-only values and colored aliases.
//! - Cold expiration selecting eager versus lazy reconstruction homes.
//! - Backend-supplied fixed call registers and bulk canonical-home inputs.
//! - Fixed shift counts and implicit division writes across exits and calls.
//!
//! # Invariants
//! Tests inspect the final allocation, before any backend emits code.
//!
//! # See also
//! - [`super::allocate`] and [`super::super::liveness`].

use super::*;
use crate::graph::ir::{BranchKind, FrameState};
use crate::graph::registers::{AARCH64, X86_64};

fn append(graph: &mut Graph, block: BlockId, kind: Kind, inputs: &[NodeId], repr: Repr) -> NodeId {
    let node = graph.add_node(kind, inputs, repr);
    graph.node_mut(node).block = Some(block);
    graph.block_mut(block).body.push(node);
    node
}

fn terminate(graph: &mut Graph, block: BlockId, kind: Kind, inputs: &[NodeId]) -> NodeId {
    let node = graph.add_node(kind, inputs, Repr::None);
    graph.node_mut(node).block = Some(block);
    graph.block_mut(block).control = Some(node);
    node
}

fn state(graph: &mut Graph, values: &[NodeId]) -> super::super::ir::FrameStateId {
    graph.add_frame_state(FrameState {
        function_id: 1,
        pc: 0,
        byte_pc: 0,
        register_count: values.len() as u16,
        registers: values
            .iter()
            .enumerate()
            .map(|(index, &value)| (index as u16, value))
            .collect(),
        caller: None,
    })
}

/// Follow the emitted allocation protocol with distinct symbolic SSA values.
/// Definition stores happen at their real definitions; reading a stale source
/// or a colored home belonging to another value fails at the consuming input.
fn symbolic_read(contents: &FxHashMap<Location, NodeId>, location: Location) -> NodeId {
    match location {
        Location::Constant(value) => value,
        _ => *contents
            .get(&location)
            .unwrap_or_else(|| panic!("uninitialized {location:?}")),
    }
}

fn symbolic_parallel_moves(contents: &mut FxHashMap<Location, NodeId>, moves: &[Move]) {
    let writes: Vec<_> = moves
        .iter()
        .map(|movement| (movement.to, symbolic_read(contents, movement.from)))
        .collect();
    for (to, value) in writes {
        contents.insert(to, value);
    }
}

fn assert_symbolic_recipe(
    graph: &Graph,
    frame: super::super::ir::FrameStateId,
    locations: &[Location],
    moves: &[Move],
    contents: &FxHashMap<Location, NodeId>,
) {
    let mut cold = contents.clone();
    symbolic_parallel_moves(&mut cold, moves);
    let values = graph.state_values(frame);
    assert_eq!(values.len(), locations.len());
    for (&expected, &location) in values.iter().zip(locations) {
        assert_eq!(
            symbolic_read(&cold, location),
            expected,
            "cold recipe at {location:?}"
        );
    }
}

fn symbolic_node(
    graph: &Graph,
    allocation: &Allocation,
    target: RegisterContract,
    node: NodeId,
    contents: &mut FxHashMap<Location, NodeId>,
) {
    let assigned = allocation.node(node);
    if assigned.skipped {
        return;
    }
    let data = graph.node(node);
    for movement in &assigned.moves {
        let value = symbolic_read(contents, movement.from);
        contents.insert(movement.to, value);
    }
    assert_eq!(data.inputs.len(), assigned.inputs.len());
    for (&expected, &location) in data.inputs.iter().zip(&assigned.inputs) {
        assert_eq!(
            symbolic_read(contents, location),
            expected,
            "input of {node:?} at {location:?}"
        );
    }
    // Model actual destructive operation writes, rather than retaining stale
    // values merely because allocator bookkeeping has unbound them.
    let constraints = data.kind.constraints(data.inputs.len(), &target);
    for &register in &constraints.fixed_gp_clobbers {
        contents.remove(&Location::Gp(register));
    }
    for &register in &assigned.gp_temps {
        contents.remove(&Location::Gp(register));
    }
    for &register in &assigned.fp_temps {
        contents.remove(&Location::Fp(register));
    }
    if data.kind.properties().call {
        contents.retain(|location, _| !matches!(location, Location::Gp(_) | Location::Fp(_)));
    }
    if let Some(result) = assigned.result {
        contents.insert(result, node);
        if allocation.definition_spills.contains(&node) {
            contents.insert(allocation.spill[&node], node);
        }
    }
    if let Some(frame) = data.eager {
        assert_symbolic_recipe(
            graph,
            frame,
            &assigned.eager,
            &assigned.eager_spills,
            contents,
        );
    }
    if let Some(frame) = data.lazy {
        assert_symbolic_recipe(
            graph,
            frame,
            &assigned.lazy,
            &assigned.lazy_spills,
            contents,
        );
    }
}

fn symbolic_block(
    graph: &Graph,
    allocation: &Allocation,
    target: RegisterContract,
    block: BlockId,
    contents: &mut FxHashMap<Location, NodeId>,
) {
    assert!(
        graph.block(block).phis.is_empty(),
        "this model follows non-phi fixture blocks"
    );
    for &node in &graph.block(block).body {
        symbolic_node(graph, allocation, target, node, contents);
    }
    if let Some(control) = graph.block(block).control {
        symbolic_node(graph, allocation, target, control, contents);
    }
}

/// A node that may collect and whose eager recipe reads `frame`, so the
/// recipe's values keep canonical homes.
fn collecting_reader(
    graph: &mut Graph,
    block: BlockId,
    frame: super::super::ir::FrameStateId,
) -> NodeId {
    let null = graph.constant(Kind::ConstTagged(otter_vm::value::tag::VALUE_NULL));
    let reader = append(graph, block, Kind::Instanceof, &[null, null], Repr::Tagged);
    graph.node_mut(reader).eager = Some(frame);
    reader
}

fn tagged(graph: &mut Graph, block: BlockId, value: NodeId) -> NodeId {
    let kind = if graph.node(value).repr.is_float() {
        Kind::Float64ToTagged
    } else {
        Kind::Int32ToTagged
    };
    append(graph, block, kind, &[value], Repr::Tagged)
}

#[test]
fn physical_assignment_uses_each_backends_register_pool_and_fixed_call_words() {
    for target in [AARCH64, X86_64] {
        let mut graph = Graph::default();
        let block = graph.new_block();
        let callee = append(
            &mut graph,
            block,
            Kind::InitialRegister(0),
            &[],
            Repr::Tagged,
        );
        let receiver = append(
            &mut graph,
            block,
            Kind::InitialRegister(1),
            &[],
            Repr::Tagged,
        );
        let call = append(
            &mut graph,
            block,
            Kind::CallJs {
                pc: 0,
                receiver: true,
                plan: crate::call_linkage::CallPlan::Generic,
                construct: false,
                allocation: None,
            },
            &[callee, receiver],
            Repr::Tagged,
        );
        terminate(&mut graph, block, Kind::Return, &[call]);
        let allocation = allocate(&graph, &[block], target);
        assert_eq!(
            allocation.node(call).inputs.as_slice(),
            &[
                Location::Gp(target.call_callee),
                Location::Gp(target.call_receiver)
            ]
        );
        assert_eq!(
            allocation.node(call).result,
            Some(Location::Gp(target.call_result))
        );
        for node in &allocation.nodes {
            for location in node
                .inputs
                .iter()
                .copied()
                .chain(node.result)
                .chain(node.gp_temps.iter().copied().map(Location::Gp))
                .chain(node.fp_temps.iter().copied().map(Location::Fp))
            {
                match location {
                    Location::Gp(register) => assert!(target.general.contains(&register)),
                    Location::Fp(register) => assert!(target.floating.contains(&register)),
                    _ => {}
                }
            }
        }
    }
}

#[test]
fn bulk_literal_inputs_use_canonical_homes_without_exhausting_either_register_file() {
    for target in [AARCH64, X86_64] {
        let mut graph = Graph::default();
        let block = graph.new_block();
        let values: Vec<_> = (0..64)
            .map(|index| {
                append(
                    &mut graph,
                    block,
                    Kind::InitialRegister(index),
                    &[],
                    Repr::Tagged,
                )
            })
            .collect();
        let eager = state(&mut graph, &values);
        let object = append(
            &mut graph,
            block,
            Kind::NewObjectLiteral,
            &values,
            Repr::Tagged,
        );
        graph.node_mut(object).eager = Some(eager);
        terminate(&mut graph, block, Kind::Return, &[object]);
        let allocation = allocate(&graph, &[block], target);
        let literal = allocation.node(object);
        assert_eq!(literal.inputs.len(), 64);
        assert_eq!(literal.gp_temps.len(), 4);
        let output = literal.result.expect("late allocation result");
        assert!(matches!(output, Location::Gp(_)));
        assert!(
            !literal
                .gp_temps
                .iter()
                .any(|&temp| output == Location::Gp(temp))
        );
        let mut homes = FxHashSet::default();
        for (index, (&value, &location)) in values.iter().zip(&literal.inputs).enumerate() {
            let Location::TaggedSlot(home) = location else {
                panic!("literal input must read its canonical tagged home");
            };
            assert!(allocation.definition_spills.contains(&value));
            assert_eq!(allocation.spill[&value], location);
            assert!(
                homes.insert(home),
                "simultaneously live inputs have distinct homes"
            );
            assert!(
                literal
                    .rooted_tagged_homes
                    .as_deref()
                    .is_some_and(|rooted| rooted.contains(&home)),
                "a collecting miss must root every still-needed initializer"
            );
            assert_eq!(literal.eager[index], location);
        }
        assert_eq!(homes.len(), 64);
    }
}

#[test]
fn fixed_shift_counts_preserve_colliding_values_and_leave_immediates_unreserved() {
    for target in [AARCH64, X86_64] {
        for kind in [
            Kind::Int32ShiftLeft,
            Kind::Int32ShiftRight,
            Kind::Int32ShiftRightLogical,
            Kind::Uint32ShiftRightToFloat64,
        ] {
            // A distinct count displaces the original left operand from CL;
            // an immediate leaves a live tagged occupant in RCX; a repeated
            // operand reads the same word twice without an unnecessary copy.
            for mode in 0..3 {
                let mut graph = Graph::default();
                let block = graph.new_block();
                let first = append(
                    &mut graph,
                    block,
                    Kind::InitialRegister(0),
                    &[],
                    if mode == 1 { Repr::Int32 } else { Repr::Tagged },
                );
                let second = append(
                    &mut graph,
                    block,
                    Kind::InitialRegister(1),
                    &[],
                    if mode == 1 { Repr::Tagged } else { Repr::Int32 },
                );
                let (sentinel, left) = if mode == 1 {
                    (second, first)
                } else {
                    (first, second)
                };
                let count = append(
                    &mut graph,
                    block,
                    Kind::InitialRegister(2),
                    &[],
                    Repr::Int32,
                );
                let right = match mode {
                    0 => count,
                    1 => graph.add_node(Kind::ConstInt32(31), &[], Repr::Int32),
                    _ => left,
                };
                let eager = state(&mut graph, &[sentinel, left, count, left]);
                let result_repr = if matches!(kind, Kind::Uint32ShiftRightToFloat64) {
                    Repr::Float64
                } else {
                    Repr::Int32
                };
                let shift = append(&mut graph, block, kind.clone(), &[left, right], result_repr);
                graph.node_mut(shift).eager = Some(eager);
                let original = tagged(&mut graph, block, left);
                let original_count = tagged(&mut graph, block, count);
                let shifted = tagged(&mut graph, block, shift);
                let call = append(
                    &mut graph,
                    block,
                    Kind::Generic {
                        pc: 0,
                        registers: Box::new([]),
                    },
                    &[sentinel, original, original_count, shifted],
                    Repr::Tagged,
                );
                let lazy = state(
                    &mut graph,
                    &[sentinel, original, original_count, shifted, call],
                );
                graph.node_mut(call).lazy = Some(lazy);
                terminate(&mut graph, block, Kind::Return, &[call]);

                let allocation = allocate(&graph, &[block], target);
                let assigned = allocation.node(shift);
                if let Some(fixed) = target.variable_shift_count {
                    if mode == 1 {
                        assert_eq!(assigned.inputs[1], Location::Constant(right));
                        assert_eq!(allocation.node(sentinel).result, Some(Location::Gp(fixed)));
                        assert!(
                            assigned
                                .live_registers
                                .contains(&(Location::Gp(fixed), Repr::Tagged))
                        );
                        assert!(
                            assigned
                                .moves
                                .iter()
                                .all(|movement| movement.to != Location::Gp(fixed)),
                            "an immediate must not displace the live count-register occupant"
                        );
                    } else {
                        assert_eq!(assigned.inputs[1], Location::Gp(fixed));
                        if mode == 0 {
                            assert_eq!(allocation.node(left).result, Some(Location::Gp(fixed)));
                            assert_ne!(assigned.inputs[0], Location::Gp(fixed));
                            assert!(
                                assigned
                                    .moves
                                    .iter()
                                    .any(|movement| movement.from == Location::Gp(fixed)
                                        && movement.to == assigned.inputs[0]),
                                "the original left operand moves before CL is overwritten"
                            );
                        } else {
                            assert_eq!(assigned.inputs[0], assigned.inputs[1]);
                        }
                    }
                } else if mode == 1 && matches!(kind, Kind::Uint32ShiftRightToFloat64) {
                    assert!(
                        matches!(assigned.inputs[1], Location::Gp(_)),
                        "ARM unsigned-to-float lowering still reads a register count"
                    );
                } else if mode == 1 {
                    assert_eq!(assigned.inputs[1], Location::Constant(right));
                }
                assert!(
                    assigned
                        .inputs
                        .iter()
                        .all(|&input| assigned.result != Some(input)),
                    "eager reconstruction keeps both original operands readable"
                );
                let mut contents = FxHashMap::default();
                symbolic_block(&graph, &allocation, target, block, &mut contents);
            }
        }
    }
}

#[test]
fn division_clobbers_preserve_original_values_for_eager_and_lazy_recipes() {
    for target in [AARCH64, X86_64] {
        for kind in [Kind::Int32Div, Kind::Int32Mod] {
            for mode in 0..3 {
                let mut graph = Graph::default();
                let block = graph.new_block();
                let tagged_word = append(
                    &mut graph,
                    block,
                    Kind::InitialRegister(0),
                    &[],
                    Repr::Tagged,
                );
                let left = append(
                    &mut graph,
                    block,
                    Kind::InitialRegister(1),
                    &[],
                    Repr::Int32,
                );
                let untagged_word = append(
                    &mut graph,
                    block,
                    Kind::InitialRegister(2),
                    &[],
                    Repr::Int32,
                );
                let right = match mode {
                    0 => untagged_word,
                    1 => graph.add_node(Kind::ConstInt32(-1), &[], Repr::Int32),
                    _ => left,
                };
                let eager = state(&mut graph, &[tagged_word, left, untagged_word, left]);
                let division = append(&mut graph, block, kind.clone(), &[left, right], Repr::Int32);
                graph.node_mut(division).eager = Some(eager);
                let original_left = tagged(&mut graph, block, left);
                let original_untagged = tagged(&mut graph, block, untagged_word);
                let quotient_or_remainder = tagged(&mut graph, block, division);
                let call = append(
                    &mut graph,
                    block,
                    Kind::Generic {
                        pc: 0,
                        registers: Box::new([]),
                    },
                    &[
                        tagged_word,
                        original_left,
                        original_untagged,
                        quotient_or_remainder,
                    ],
                    Repr::Tagged,
                );
                let lazy = state(
                    &mut graph,
                    &[
                        tagged_word,
                        original_left,
                        original_untagged,
                        quotient_or_remainder,
                        call,
                    ],
                );
                graph.node_mut(call).lazy = Some(lazy);
                terminate(&mut graph, block, Kind::Return, &[call]);

                let allocation = allocate(&graph, &[block], target);
                let assigned = allocation.node(division);
                if let Some(pair) = target.integer_division {
                    assert_eq!(
                        allocation.node(tagged_word).result,
                        Some(Location::Gp(pair.quotient))
                    );
                    assert_eq!(
                        allocation.node(untagged_word).result,
                        Some(Location::Gp(pair.remainder))
                    );
                    for value in [tagged_word, untagged_word] {
                        assert!(
                            allocation.definition_spills.contains(&value),
                            "an implicit write requires the original value at its definition home"
                        );
                    }
                    assert!(matches!(
                        allocation.spill[&tagged_word],
                        Location::TaggedSlot(_)
                    ));
                    assert!(matches!(
                        allocation.spill[&untagged_word],
                        Location::UntaggedSlot(_)
                    ));
                    assert!(
                        assigned
                            .inputs
                            .iter()
                            .all(|location| matches!(location, Location::Gp(register)
                            if *register != pair.quotient && *register != pair.remainder))
                    );
                    assert_eq!(
                        assigned.result,
                        Some(Location::Gp(if matches!(kind, Kind::Int32Div) {
                            pair.quotient
                        } else {
                            pair.remainder
                        }))
                    );
                    assert!(assigned.gp_temps.is_empty());
                    assert!(
                        assigned.eager.iter().all(|&location| location
                            != Location::Gp(pair.quotient)
                            && location != Location::Gp(pair.remainder)),
                        "even post-division exits must not read either overwritten implicit word"
                    );
                } else {
                    assert_eq!(
                        assigned.gp_temps.len(),
                        1,
                        "ARM retains its ordinary division temporary"
                    );
                    let temporary = Location::Gp(assigned.gp_temps[0]);
                    assert!(!assigned.inputs.contains(&temporary));
                    assert_ne!(assigned.result, Some(temporary));
                    // A materialized constant that dies here may supply the
                    // result register. ARM computes in x16/x17 and commits
                    // only after every guard; the symbolic recipe below
                    // checks preservation of the original eager values.
                }
                assert!(
                    assigned.eager_spills.is_empty(),
                    "a guard's exit reads its values where they are"
                );
                assert_eq!(
                    assigned.eager[1], assigned.eager[3],
                    "repeated frame bindings read one location"
                );
                let mut contents = FxHashMap::default();
                symbolic_block(&graph, &allocation, target, block, &mut contents);
            }
        }
    }
}

#[test]
fn fixed_arithmetic_survives_pressure_call_reload_and_backedge_home_restoration() {
    for target in [AARCH64, X86_64] {
        let mut graph = Graph::default();
        let entry = graph.new_block();
        let header = graph.new_block();
        graph.block_mut(header).predecessors = vec![entry, header];
        graph.block_mut(header).is_loop = true;
        let values: Vec<_> = (0..target.general.len() + 8)
            .map(|index| {
                append(
                    &mut graph,
                    entry,
                    Kind::InitialRegister(index as u16),
                    &[],
                    if index % 2 == 0 {
                        Repr::Tagged
                    } else {
                        Repr::Int32
                    },
                )
            })
            .collect();
        let entry_control = terminate(&mut graph, entry, Kind::Jump(header), &[]);
        let eager = state(&mut graph, &values);
        let division = append(
            &mut graph,
            header,
            Kind::Int32Div,
            &[values[1], values[3]],
            Repr::Int32,
        );
        graph.node_mut(division).eager = Some(eager);
        let shift = append(
            &mut graph,
            header,
            Kind::Int32ShiftRightLogical,
            &[division, values[5]],
            Repr::Int32,
        );
        graph.node_mut(shift).eager = Some(eager);
        let point = append(
            &mut graph,
            header,
            Kind::Instanceof,
            &[values[0], values[2]],
            Repr::Tagged,
        );
        graph.node_mut(point).eager = Some(eager);
        let shifted = tagged(&mut graph, header, shift);
        let tagged_values: Vec<_> = values.iter().step_by(2).copied().chain([shifted]).collect();
        let call = append(
            &mut graph,
            header,
            Kind::Generic {
                pc: 0,
                registers: Box::new([]),
            },
            &tagged_values,
            Repr::Tagged,
        );
        graph.node_mut(call).eager = Some(eager);
        let lazy = state(
            &mut graph,
            &values.iter().copied().chain([call]).collect::<Vec<_>>(),
        );
        graph.node_mut(call).lazy = Some(lazy);
        append(&mut graph, header, Kind::CheckNotHole, &[call], Repr::None);
        let reloaded = append(
            &mut graph,
            header,
            Kind::Int32Mod,
            &[values[3], values[1]],
            Repr::Int32,
        );
        graph.node_mut(reloaded).eager = Some(eager);
        let retagged = tagged(&mut graph, header, reloaded);
        append(
            &mut graph,
            header,
            Kind::CheckNotHole,
            &[retagged],
            Repr::None,
        );
        for &value in values.iter().rev() {
            if graph.node(value).repr == Repr::Tagged {
                append(&mut graph, header, Kind::CheckNotHole, &[value], Repr::None);
            } else {
                let word = tagged(&mut graph, header, value);
                append(&mut graph, header, Kind::CheckNotHole, &[word], Repr::None);
            }
        }
        let poll = terminate(&mut graph, header, Kind::JumpLoop(header), &[]);
        graph.node_mut(poll).eager = Some(eager);
        let allocation = allocate(&graph, &[entry, header], target);
        let homes: FxHashSet<_> = values.iter().map(|value| allocation.spill[value]).collect();
        assert_eq!(
            homes.len(),
            values.len(),
            "all loop-live values retain distinct canonical homes"
        );
        for &value in &values {
            assert!(matches!(
                (graph.node(value).repr, allocation.spill[&value]),
                (Repr::Tagged, Location::TaggedSlot(_)) | (Repr::Int32, Location::UntaggedSlot(_))
            ));
        }
        if let Some(pair) = target.integer_division {
            for node in [division, reloaded] {
                assert!(
                    allocation
                        .node(node)
                        .inputs
                        .iter()
                        .all(|location| matches!(location, Location::Gp(register)
                        if *register != pair.quotient && *register != pair.remainder))
                );
            }
        }
        assert!(
            allocation
                .node(reloaded)
                .moves
                .iter()
                .filter(|movement| matches!(movement.from, Location::UntaggedSlot(_)))
                .count()
                >= 2,
            "the first arithmetic operation after a call reloads both original operands"
        );
        assert_memory_backedge_restores(&allocation, &values, entry_control, poll, header);
        let mut contents = FxHashMap::default();
        symbolic_block(&graph, &allocation, target, entry, &mut contents);
        let incoming: Vec<_> = allocation
            .node(entry_control)
            .live_registers
            .iter()
            .map(|&(location, _)| (location, symbolic_read(&contents, location)))
            .collect();
        symbolic_block(&graph, &allocation, target, header, &mut contents);
        symbolic_parallel_moves(&mut contents, &allocation.edges[&(header, header)].moves);
        for (location, value) in incoming {
            assert_eq!(
                symbolic_read(&contents, location),
                value,
                "backedge restores {location:?}"
            );
        }
        symbolic_block(&graph, &allocation, target, header, &mut contents);
    }
}

#[test]
fn fixed_arithmetic_and_return_follow_named_backend_roles() {
    let target = RegisterContract {
        variable_shift_count: Some(8),
        integer_division: Some(crate::graph::registers::IntDivisionRegisters {
            quotient: 6,
            remainder: 7,
        }),
        call_result: 12,
        ..X86_64
    };
    let mut graph = Graph::default();
    let block = graph.new_block();
    let left = append(
        &mut graph,
        block,
        Kind::InitialRegister(0),
        &[],
        Repr::Int32,
    );
    let right = append(
        &mut graph,
        block,
        Kind::InitialRegister(1),
        &[],
        Repr::Int32,
    );
    let eager = state(&mut graph, &[left, right]);
    let shift = append(
        &mut graph,
        block,
        Kind::Int32ShiftLeft,
        &[left, right],
        Repr::Int32,
    );
    graph.node_mut(shift).eager = Some(eager);
    let division = append(
        &mut graph,
        block,
        Kind::Int32Div,
        &[shift, right],
        Repr::Int32,
    );
    graph.node_mut(division).eager = Some(eager);
    let returned = tagged(&mut graph, block, division);
    let control = terminate(&mut graph, block, Kind::Return, &[returned]);
    let allocation = allocate(&graph, &[block], target);
    assert_eq!(allocation.node(shift).inputs[1], Location::Gp(8));
    assert_eq!(allocation.node(division).result, Some(Location::Gp(6)));
    assert!(
        allocation.node(division).inputs.iter().all(
            |location| matches!(location, Location::Gp(register) if ![6, 7].contains(register))
        )
    );
    assert_eq!(
        allocation.node(control).inputs.as_slice(),
        &[Location::Gp(12)]
    );
    let mut contents = FxHashMap::default();
    symbolic_block(&graph, &allocation, target, block, &mut contents);
}

#[test]
fn fixed_count_eviction_keeps_an_exit_only_occupant_under_full_pressure() {
    let mut graph = Graph::default();
    let block = graph.new_block();
    let values: Vec<_> = (0..X86_64.general.len())
        .map(|index| {
            append(
                &mut graph,
                block,
                Kind::InitialRegister(index as u16),
                &[],
                if [3, 8].contains(&index) {
                    Repr::Int32
                } else {
                    Repr::Tagged
                },
            )
        })
        .collect();
    let eager = state(&mut graph, &values);
    let shift = append(
        &mut graph,
        block,
        Kind::Int32ShiftRightLogical,
        &[values[3], values[8]],
        Repr::Int32,
    );
    graph.node_mut(shift).eager = Some(eager);
    let returned = tagged(&mut graph, block, shift);
    terminate(&mut graph, block, Kind::Return, &[returned]);
    let allocation = allocate(&graph, &[block], X86_64);
    assert_eq!(allocation.node(values[1]).result, Some(Location::Gp(1)));
    assert!(allocation.definition_spills.contains(&values[1]));
    assert!(matches!(
        allocation.spill[&values[1]],
        Location::TaggedSlot(_)
    ));
    assert_eq!(allocation.node(shift).inputs[1], Location::Gp(1));
    assert!(
        allocation
            .node(shift)
            .moves
            .iter()
            .all(|movement| movement.from != Location::Gp(1)),
        "with no free word, the displaced exit-only occupant lives in its definition home"
    );
    let liveness = crate::graph::liveness::Liveness::compute(&graph, &[block]);
    assert!(
        !liveness.is_live_after(shift, values[1]),
        "the frame state is its final use"
    );
    let mut contents = FxHashMap::default();
    symbolic_block(&graph, &allocation, X86_64, block, &mut contents);
}

#[test]
fn division_evicts_both_exit_only_words_before_materializing_a_constant_operand() {
    for kind in [Kind::Int32Div, Kind::Int32Mod] {
        let mut graph = Graph::default();
        let block = graph.new_block();
        let tagged_word = append(
            &mut graph,
            block,
            Kind::InitialRegister(0),
            &[],
            Repr::Tagged,
        );
        let left = append(
            &mut graph,
            block,
            Kind::InitialRegister(1),
            &[],
            Repr::Int32,
        );
        let untagged_word = append(
            &mut graph,
            block,
            Kind::InitialRegister(2),
            &[],
            Repr::Int32,
        );
        let right = graph.add_node(Kind::ConstInt32(-1), &[], Repr::Int32);
        let eager = state(&mut graph, &[tagged_word, left, untagged_word]);
        let division = append(&mut graph, block, kind, &[left, right], Repr::Int32);
        graph.node_mut(division).eager = Some(eager);
        let returned = tagged(&mut graph, block, division);
        terminate(&mut graph, block, Kind::Return, &[returned]);
        let allocation = allocate(&graph, &[block], X86_64);
        let liveness = crate::graph::liveness::Liveness::compute(&graph, &[block]);
        for value in [tagged_word, untagged_word] {
            assert!(!liveness.is_live_after(division, value));
            assert!(allocation.definition_spills.contains(&value));
        }
        assert!(matches!(
            allocation.spill[&tagged_word],
            Location::TaggedSlot(_)
        ));
        assert!(matches!(
            allocation.spill[&untagged_word],
            Location::UntaggedSlot(_)
        ));
        assert!(allocation.node(division).inputs.iter().all(
            |location| matches!(location, Location::Gp(register) if ![0, 2].contains(register))
        ));
        assert!(allocation.node(division).moves.contains(&Move {
            from: Location::Constant(right),
            to: allocation.node(division).inputs[1],
        }));
        let mut contents = FxHashMap::default();
        symbolic_block(&graph, &allocation, X86_64, block, &mut contents);
    }
}

#[test]
fn a_two_address_result_can_reuse_a_dying_count_only_after_inputs_are_read() {
    for kind in [Kind::Int32ShiftLeft, Kind::Int32ShiftRight] {
        let mut graph = Graph::default();
        let block = graph.new_block();
        let sentinel = append(
            &mut graph,
            block,
            Kind::InitialRegister(0),
            &[],
            Repr::Tagged,
        );
        let count = append(
            &mut graph,
            block,
            Kind::InitialRegister(1),
            &[],
            Repr::Int32,
        );
        let left = append(
            &mut graph,
            block,
            Kind::InitialRegister(2),
            &[],
            Repr::Int32,
        );
        let shift = append(&mut graph, block, kind, &[left, count], Repr::Int32);
        let shifted = tagged(&mut graph, block, shift);
        let call = append(
            &mut graph,
            block,
            Kind::Generic {
                pc: 0,
                registers: Box::new([]),
            },
            &[sentinel, shifted],
            Repr::Tagged,
        );
        terminate(&mut graph, block, Kind::Return, &[call]);
        let allocation = allocate(&graph, &[block], X86_64);
        assert_eq!(
            allocation.node(shift).inputs.as_slice(),
            &[Location::Gp(2), Location::Gp(1)]
        );
        assert_eq!(
            allocation.node(shift).result,
            Some(Location::Gp(1)),
            "the backend must read CL before committing a result to that same word"
        );
        let mut contents = FxHashMap::default();
        symbolic_block(&graph, &allocation, X86_64, block, &mut contents);
    }
}

/// The pressure fixtures' entry values are successive definitions in one
/// register class; their last result writes give the header's incoming map.
/// Check that actual entry registers agree, then require every memory-only
/// occupant at the poll to be restored from its own home on the backedge.
fn assert_memory_backedge_restores(
    allocation: &Allocation,
    values: &[NodeId],
    entry_control: NodeId,
    poll: NodeId,
    header: BlockId,
) {
    let float = matches!(allocation.node(values[0]).result, Some(Location::Fp(_)));
    let same_class = |location: Location| {
        matches!(location, Location::Fp(_)) == float
            && matches!(location, Location::Gp(_) | Location::Fp(_))
    };
    let mut incoming = FxHashMap::default();
    for &value in values {
        let node = allocation.node(value);
        let register = node.result.expect("an entry definition has a register");
        assert!(same_class(register));
        assert!(
            node.moves.iter().all(|movement| !same_class(movement.to)),
            "only the entry definitions write registers in this class",
        );
        incoming.insert(register, value);
    }
    assert_eq!(
        incoming.keys().copied().collect::<FxHashSet<_>>(),
        allocation
            .node(entry_control)
            .live_registers
            .iter()
            .map(|&(location, _)| location)
            .filter(|&location| same_class(location))
            .collect::<FxHashSet<_>>(),
        "the entry's actual live registers must match its last definition writes",
    );
    let backedge = &allocation.edges[&(header, header)];
    let mut restored = 0;
    for (register, value) in incoming {
        let home = allocation.spill[&value];
        if allocation
            .node(poll)
            .live_homes
            .iter()
            .any(|&(_, occupied_home)| occupied_home == home)
        {
            continue;
        }
        restored += 1;
        assert!(allocation.definition_spills.contains(&value));
        assert!(
            backedge.moves.contains(&Move {
                from: home,
                to: register,
            }),
            "memory-only entry value {value:?} must return from {home:?} to {register:?}",
        );
    }
    assert!(
        restored > 0,
        "the fixture must actually restore memory-only entry values on its backedge",
    );
}

#[test]
fn collecting_slow_path_and_later_call_share_one_home() {
    let mut graph = Graph::default();
    let block = graph.new_block();
    let value = append(
        &mut graph,
        block,
        Kind::InitialRegister(0),
        &[],
        Repr::Tagged,
    );
    let target = append(
        &mut graph,
        block,
        Kind::InitialRegister(1),
        &[],
        Repr::Tagged,
    );
    let point = append(
        &mut graph,
        block,
        Kind::Instanceof,
        &[value, target],
        Repr::Tagged,
    );
    let call = append(
        &mut graph,
        block,
        Kind::Generic {
            pc: 0,
            registers: Box::new([]),
        },
        &[],
        Repr::None,
    );
    terminate(&mut graph, block, Kind::Return, &[value]);
    let allocation = allocate(&graph, &[block], AARCH64);
    let home = allocation.spill[&value];
    assert!(matches!(home, Location::TaggedSlot(_)));
    assert!(
        allocation
            .node(point)
            .live_homes
            .iter()
            .any(|&(_, slow_home)| slow_home == home)
    );
    assert!(!allocation.node(call).skipped);
    assert!(allocation.node(point).gp_temps.iter().all(|&temporary| {
        !allocation
            .node(point)
            .inputs
            .contains(&Location::Gp(temporary))
    }));
}

#[test]
fn boundary_roots_a_live_memory_value_and_not_a_dead_one() {
    let mut graph = Graph::default();
    let block = graph.new_block();
    let alive = append(
        &mut graph,
        block,
        Kind::InitialRegister(0),
        &[],
        Repr::Tagged,
    );
    let dead = append(
        &mut graph,
        block,
        Kind::InitialRegister(1),
        &[],
        Repr::Tagged,
    );
    let call_state = state(&mut graph, &[alive, dead]);
    let call = append(
        &mut graph,
        block,
        Kind::Generic {
            pc: 0,
            registers: Box::new([]),
        },
        &[],
        Repr::None,
    );
    graph.node_mut(call).eager = Some(call_state);
    let check_state = state(&mut graph, &[dead]);
    let check = append(&mut graph, block, Kind::CheckNotHole, &[dead], Repr::None);
    graph.node_mut(check).eager = Some(check_state);
    let null = graph.constant(Kind::ConstTagged(otter_vm::value::tag::VALUE_NULL));
    let point = append(
        &mut graph,
        block,
        Kind::Instanceof,
        &[null, null],
        Repr::Tagged,
    );
    terminate(&mut graph, block, Kind::Return, &[alive]);

    let allocation = allocate(&graph, &[block], AARCH64);
    let Location::TaggedSlot(alive_slot) = allocation.spill[&alive] else {
        panic!("tagged live home")
    };
    let Location::TaggedSlot(dead_slot) = allocation.spill[&dead] else {
        panic!("tagged dead home")
    };
    assert_ne!(
        alive_slot, dead_slot,
        "overlapping call values have distinct homes"
    );
    assert!(allocation.definition_spills.contains(&alive));
    assert!(allocation.definition_spills.contains(&dead));
    assert!(
        allocation
            .node(point)
            .live_homes
            .iter()
            .all(|&(_, home)| home != allocation.spill[&alive]),
        "the live value must actually remain in memory, absent from the register-save list",
    );
    let rooted = allocation
        .node(point)
        .rooted_tagged_homes
        .as_deref()
        .expect("a collecting boundary has a root plan");
    assert!(rooted.contains(&alive_slot));
    assert!(!rooted.contains(&dead_slot));
    assert!(
        allocation.node(check).rooted_tagged_homes.is_none(),
        "a NoAlloc guard is no collecting boundary"
    );
}

#[test]
fn boundary_roots_the_live_occupant_of_a_reused_slot() {
    let mut graph = Graph::default();
    let block = graph.new_block();
    let first = append(
        &mut graph,
        block,
        Kind::InitialRegister(0),
        &[],
        Repr::Tagged,
    );
    let first_state = state(&mut graph, &[first]);
    let first_check = collecting_reader(&mut graph, block, first_state);
    let second = append(
        &mut graph,
        block,
        Kind::InitialRegister(1),
        &[],
        Repr::Tagged,
    );
    let second_state = state(&mut graph, &[second]);
    let second_check = append(&mut graph, block, Kind::CheckNotHole, &[second], Repr::None);
    graph.node_mut(second_check).eager = Some(second_state);
    let null = graph.constant(Kind::ConstTagged(otter_vm::value::tag::VALUE_NULL));
    let point = append(
        &mut graph,
        block,
        Kind::Instanceof,
        &[null, null],
        Repr::Tagged,
    );
    graph.node_mut(point).eager = Some(second_state);
    terminate(&mut graph, block, Kind::Return, &[null]);

    let allocation = allocate(&graph, &[block], AARCH64);
    assert_eq!(allocation.spill[&first], allocation.spill[&second]);
    assert_eq!(
        allocation.tagged_slots, 1,
        "the fixture actually colors the two values together"
    );
    let Location::TaggedSlot(slot) = allocation.spill[&second] else {
        panic!("tagged eager-only home")
    };
    assert_eq!(
        allocation.node(point).rooted_tagged_homes.as_deref(),
        Some(&[slot][..]),
        "a dead SSA alias cannot hide the live eager-only occupant"
    );
}

#[test]
fn call_roots_current_inputs_and_lazy_only_state() {
    let mut graph = Graph::default();
    let block = graph.new_block();
    let argument = append(
        &mut graph,
        block,
        Kind::InitialRegister(0),
        &[],
        Repr::Tagged,
    );
    let lazy_only = append(
        &mut graph,
        block,
        Kind::InitialRegister(1),
        &[],
        Repr::Tagged,
    );
    let dead = append(
        &mut graph,
        block,
        Kind::InitialRegister(2),
        &[],
        Repr::Tagged,
    );
    let saved = state(&mut graph, &[argument, lazy_only, dead]);
    let spill = append(
        &mut graph,
        block,
        Kind::Generic {
            pc: 0,
            registers: Box::new([]),
        },
        &[],
        Repr::None,
    );
    graph.node_mut(spill).eager = Some(saved);
    let callee = graph.constant(Kind::ConstTagged(
        otter_vm::value::tag::FUNCTION_ID_TAG | (1 << 16),
    ));
    let call = append(
        &mut graph,
        block,
        Kind::CallJs {
            pc: 0,
            plan: crate::call_linkage::CallPlan::Generic,
            construct: false,
            receiver: false,
            allocation: None,
        },
        &[callee, argument],
        Repr::Tagged,
    );
    let lazy_state = state(&mut graph, &[lazy_only]);
    graph.node_mut(call).lazy = Some(lazy_state);
    let null = graph.constant(Kind::ConstTagged(otter_vm::value::tag::VALUE_NULL));
    terminate(&mut graph, block, Kind::Return, &[null]);

    let allocation = allocate(&graph, &[block], AARCH64);
    assert!(
        allocation.node(call).live_homes.is_empty(),
        "calls use already spilled homes"
    );
    let rooted = allocation
        .node(call)
        .rooted_tagged_homes
        .as_deref()
        .expect("a call has a root plan");
    for protected in [argument, lazy_only] {
        let Location::TaggedSlot(slot) = allocation.spill[&protected] else {
            panic!("tagged input/state home")
        };
        assert!(rooted.contains(&slot));
    }
    let Location::TaggedSlot(dead_slot) = allocation.spill[&dead] else {
        panic!("tagged dead home")
    };
    assert!(!rooted.contains(&dead_slot));
}

#[test]
fn boundary_does_not_root_an_unproduced_collecting_result() {
    let mut graph = Graph::default();
    let block = graph.new_block();
    let first = append(
        &mut graph,
        block,
        Kind::InitialRegister(0),
        &[],
        Repr::Tagged,
    );
    let first_state = state(&mut graph, &[first]);
    let first_check = collecting_reader(&mut graph, block, first_state);
    let null = graph.constant(Kind::ConstTagged(otter_vm::value::tag::VALUE_NULL));
    let point = append(
        &mut graph,
        block,
        Kind::Instanceof,
        &[null, null],
        Repr::Tagged,
    );
    let result_state = state(&mut graph, &[point]);
    collecting_reader(&mut graph, block, result_state);
    terminate(&mut graph, block, Kind::Return, &[point]);

    let allocation = allocate(&graph, &[block], AARCH64);
    assert_eq!(allocation.spill[&first], allocation.spill[&point]);
    let Location::TaggedSlot(slot) = allocation.spill[&point] else {
        panic!("tagged result home")
    };
    assert!(
        !allocation
            .node(point)
            .rooted_tagged_homes
            .as_deref()
            .expect("a collecting boundary has a root plan")
            .contains(&slot),
        "the future result cannot root the old occupant before it is produced"
    );
}

#[test]
fn deopt_homes_separate_tagged_int32_and_float64_values() {
    let mut graph = Graph::default();
    let block = graph.new_block();
    let tagged = append(
        &mut graph,
        block,
        Kind::InitialRegister(0),
        &[],
        Repr::Tagged,
    );
    let integer = append(
        &mut graph,
        block,
        Kind::CheckedTaggedToInt32,
        &[tagged],
        Repr::Int32,
    );
    let float = append(
        &mut graph,
        block,
        Kind::Int32ToFloat64,
        &[integer],
        Repr::Float64,
    );
    let constant = graph.add_node(Kind::ConstInt32(42), &[], Repr::Int32);
    let eager = state(&mut graph, &[tagged, integer, float, constant]);
    let point = append(
        &mut graph,
        block,
        Kind::Instanceof,
        &[tagged, tagged],
        Repr::Tagged,
    );
    graph.node_mut(point).eager = Some(eager);
    terminate(&mut graph, block, Kind::Return, &[tagged]);
    let allocation = allocate(&graph, &[block], AARCH64);
    assert!(matches!(
        allocation.node(point).eager[0],
        Location::TaggedSlot(_)
    ));
    assert!(matches!(
        allocation.node(point).eager[1],
        Location::UntaggedSlot(_)
    ));
    assert!(matches!(
        allocation.node(point).eager[2],
        Location::UntaggedSlot(_)
    ));
    assert_eq!(
        allocation.node(point).eager[3],
        Location::Constant(constant)
    );
    assert_ne!(allocation.spill[&integer], allocation.spill[&float]);
}

#[test]
fn non_overlapping_definitions_reuse_their_canonical_home() {
    let mut graph = Graph::default();
    let block = graph.new_block();
    let first = append(
        &mut graph,
        block,
        Kind::InitialRegister(0),
        &[],
        Repr::Tagged,
    );
    let first_state = state(&mut graph, &[first]);
    let first_check = collecting_reader(&mut graph, block, first_state);
    let second = append(
        &mut graph,
        block,
        Kind::InitialRegister(1),
        &[],
        Repr::Tagged,
    );
    let second_state = state(&mut graph, &[second]);
    collecting_reader(&mut graph, block, second_state);
    terminate(&mut graph, block, Kind::Return, &[second]);
    let allocation = allocate(&graph, &[block], AARCH64);
    assert_eq!(allocation.spill[&first], allocation.spill[&second]);
    assert_eq!(allocation.tagged_slots, 1);
    assert!(!allocation.definition_spills.contains(&first));
    assert_eq!(
        allocation.node(first_check).eager_spills,
        vec![Move {
            from: allocation
                .node(first)
                .result
                .expect("initial value in a register"),
            to: allocation.spill[&first],
        }],
        "an exit-only value populates its home on the cold exit",
    );
}

#[test]
fn inner_loop_poll_does_not_save_a_value_read_only_on_another_path() {
    let mut graph = Graph::default();
    let entry = graph.new_block();
    let inner = graph.new_block();
    let outer_exit = graph.new_block();
    graph.block_mut(inner).predecessors = vec![entry, inner];
    graph.block_mut(inner).is_loop = true;
    graph.block_mut(outer_exit).predecessors = vec![entry];
    let value = append(
        &mut graph,
        entry,
        Kind::InitialRegister(0),
        &[],
        Repr::Tagged,
    );
    let condition = graph.add_node(
        Kind::ConstTagged(otter_vm::value::tag::VALUE_TRUE),
        &[],
        Repr::Tagged,
    );
    terminate(
        &mut graph,
        entry,
        Kind::Branch {
            kind: BranchKind::Truthy,
            if_true: inner,
            if_false: outer_exit,
        },
        &[condition],
    );
    let empty = state(&mut graph, &[]);
    let poll = terminate(&mut graph, inner, Kind::JumpLoop(inner), &[]);
    graph.node_mut(poll).eager = Some(empty);
    let exit_state = state(&mut graph, &[value]);
    collecting_reader(&mut graph, outer_exit, exit_state);
    terminate(&mut graph, outer_exit, Kind::Return, &[value]);
    let allocation = allocate(&graph, &[entry, inner, outer_exit], AARCH64);
    assert!(matches!(allocation.spill[&value], Location::TaggedSlot(_)));
    assert!(allocation.node(poll).live_homes.is_empty());
}

#[test]
fn collecting_slow_path_rematerializes_live_constants() {
    let mut graph = Graph::default();
    let block = graph.new_block();
    let value = append(
        &mut graph,
        block,
        Kind::InitialRegister(0),
        &[],
        Repr::Tagged,
    );
    let constant = graph.add_node(
        Kind::ConstTagged(otter_vm::value::tag::VALUE_TRUE),
        &[],
        Repr::Tagged,
    );
    let point = append(
        &mut graph,
        block,
        Kind::Instanceof,
        &[value, constant],
        Repr::Tagged,
    );
    let result = append(
        &mut graph,
        block,
        Kind::LogicalNot,
        &[constant],
        Repr::Tagged,
    );
    terminate(&mut graph, block, Kind::Return, &[result]);
    let allocation = allocate(&graph, &[block], AARCH64);
    assert!(
        allocation
            .node(point)
            .live_homes
            .iter()
            .any(|&(_, home)| home == Location::Constant(constant))
    );
    assert!(!allocation.spill.contains_key(&constant));
}

#[test]
fn poll_saves_eager_state_values_that_die_on_the_backedge() {
    let mut graph = Graph::default();
    let header = graph.new_block();
    graph.block_mut(header).predecessors = vec![header];
    graph.block_mut(header).is_loop = true;
    // This definition runs again at the header, so the old value is dead
    // after the edge, while an interrupted poll still owes its eager state.
    let value = append(
        &mut graph,
        header,
        Kind::InitialRegister(0),
        &[],
        Repr::Tagged,
    );
    let eager = state(&mut graph, &[value]);
    let poll = terminate(&mut graph, header, Kind::JumpLoop(header), &[]);
    graph.node_mut(poll).eager = Some(eager);
    let liveness = crate::graph::liveness::Liveness::compute(&graph, &[header]);
    assert!(!liveness.is_live_after(poll, value));
    let allocation = allocate(&graph, &[header], AARCH64);
    let register = allocation.node(value).result.expect("value in a register");
    let home = allocation.spill[&value];
    assert!(allocation.node(poll).live_homes.contains(&(register, home)));
    assert_eq!(
        allocation.node(poll).eager_spills,
        vec![Move {
            from: register,
            to: home
        }]
    );
    assert!(!allocation.definition_spills.contains(&value));
}

#[test]
fn register_pressure_preserves_distinct_live_homes_across_collection_and_backedge() {
    for target in [AARCH64, X86_64] {
        let mut graph = Graph::default();
        let entry = graph.new_block();
        let header = graph.new_block();
        graph.block_mut(header).predecessors = vec![entry, header];
        graph.block_mut(header).is_loop = true;
        let values: Vec<NodeId> = (0..target.general.len() + 8)
            .map(|index| {
                append(
                    &mut graph,
                    entry,
                    Kind::InitialRegister(index as u16),
                    &[],
                    Repr::Tagged,
                )
            })
            .collect();
        let entry_control = terminate(&mut graph, entry, Kind::Jump(header), &[]);
        let eager = state(&mut graph, &values);
        let point = append(
            &mut graph,
            header,
            Kind::Instanceof,
            &values[..2],
            Repr::Tagged,
        );
        graph.node_mut(point).eager = Some(eager);
        // Reverse the uses so the exhausted register file at the backedge is
        // different from the header's entry state. Forward uses happen to
        // recreate that state under equal next-use ties, requiring no reload.
        for &value in values.iter().rev() {
            append(&mut graph, header, Kind::CheckNotHole, &[value], Repr::None);
        }
        let poll = terminate(&mut graph, header, Kind::JumpLoop(header), &[]);
        graph.node_mut(poll).eager = Some(eager);

        let liveness = crate::graph::liveness::Liveness::compute(&graph, &[entry, header]);
        let allocation = allocate(&graph, &[entry, header], target);
        let homes: FxHashSet<Location> =
            values.iter().map(|value| allocation.spill[value]).collect();
        assert_eq!(
            homes.len(),
            values.len(),
            "overlapping values cannot share a home"
        );
        assert!(
            homes
                .iter()
                .all(|home| matches!(home, Location::TaggedSlot(_)))
        );
        assert!(
            values
                .iter()
                .filter(|&&value| allocation.definition_spills.contains(&value))
                .count()
                >= values.len() - target.general.len(),
            "exhaustion must force real definition spills, not only cold exit homes",
        );
        assert_eq!(
            allocation.node(point).eager,
            values
                .iter()
                .map(|value| allocation.spill[value])
                .collect::<Vec<_>>(),
        );
        for &node in &[point, poll] {
            assert!(
                values
                    .iter()
                    .all(|&value| liveness.is_live_after(node, value))
            );
            let saved = &allocation.node(node).live_homes;
            assert!(
                !saved.is_empty(),
                "register values must be saved through their homes"
            );
            assert!(
                saved.len() < values.len(),
                "some live values must already be spilled"
            );
            assert!(
                saved.iter().all(|&(from, home)| {
                    matches!(from, Location::Gp(_)) && homes.contains(&home)
                })
            );
        }
        assert_memory_backedge_restores(&allocation, &values, entry_control, poll, header);
    }
}

#[test]
fn float_pressure_preserves_untagged_homes_through_collection_call_reload_and_backedge() {
    for target in [AARCH64, X86_64] {
        let mut graph = Graph::default();
        let entry = graph.new_block();
        let header = graph.new_block();
        graph.block_mut(header).predecessors = vec![entry, header];
        graph.block_mut(header).is_loop = true;
        let companion = append(
            &mut graph,
            entry,
            Kind::InitialRegister(0),
            &[],
            Repr::Tagged,
        );
        let callee = append(
            &mut graph,
            entry,
            Kind::InitialRegister(1),
            &[],
            Repr::Tagged,
        );
        let mut floats = Vec::new();
        for index in 0..target.floating.len() + 8 {
            let tagged = append(
                &mut graph,
                entry,
                Kind::InitialRegister(index as u16 + 2),
                &[],
                Repr::Tagged,
            );
            let mut before = vec![companion, callee, tagged];
            before.extend(floats.iter().copied());
            let eager = state(&mut graph, &before);
            let integer = append(
                &mut graph,
                entry,
                Kind::CheckedTaggedToInt32,
                &[tagged],
                Repr::Int32,
            );
            graph.node_mut(integer).eager = Some(eager);
            floats.push(append(
                &mut graph,
                entry,
                Kind::Int32ToFloat64,
                &[integer],
                Repr::Float64,
            ));
        }
        let entry_control = terminate(&mut graph, entry, Kind::Jump(header), &[]);
        let mut values = vec![companion, callee];
        values.extend(floats.iter().copied());
        let eager = state(&mut graph, &values);
        let point = append(
            &mut graph,
            header,
            Kind::Instanceof,
            &[companion, callee],
            Repr::Tagged,
        );
        graph.node_mut(point).eager = Some(eager);
        let call = append(
            &mut graph,
            header,
            Kind::Generic {
                pc: 0,
                registers: Box::new([]),
            },
            &[],
            Repr::None,
        );
        graph.node_mut(call).eager = Some(eager);
        // Reload one actual floating value after the clobbering call. Its box
        // is consumed by a check, so neither the reload nor the boxing is dead.
        let reloaded = *floats.last().expect("floating values");
        let boxed = append(
            &mut graph,
            header,
            Kind::Float64ToTagged,
            &[reloaded],
            Repr::Tagged,
        );
        let checked = append(&mut graph, header, Kind::CheckNotHole, &[boxed], Repr::None);
        graph.node_mut(checked).eager = Some(eager);
        let poll = terminate(&mut graph, header, Kind::JumpLoop(header), &[]);
        graph.node_mut(poll).eager = Some(eager);

        let allocation = allocate(&graph, &[entry, header], target);
        let liveness = crate::graph::liveness::Liveness::compute(&graph, &[entry, header]);
        let float_homes: FxHashSet<Location> =
            floats.iter().map(|value| allocation.spill[value]).collect();
        assert_eq!(float_homes.len(), floats.len());
        assert!(
            float_homes
                .iter()
                .all(|home| matches!(home, Location::UntaggedSlot(_)))
        );
        let tagged_home = allocation.spill[&companion];
        assert!(matches!(tagged_home, Location::TaggedSlot(_)));
        assert!(
        allocation
            .node(point)
            .live_homes
            .iter()
            .any(|&(register, home)| matches!(register, Location::Gp(_)) && home == tagged_home),
        "a collecting operation must preserve its live tagged companion",
    );
        let saved_floats: Vec<_> = allocation
            .node(point)
            .live_homes
            .iter()
            .filter(|&&(register, _)| matches!(register, Location::Fp(_)))
            .copied()
            .collect();
        assert_eq!(saved_floats.len(), target.floating.len());
        assert!(saved_floats.len() < floats.len());
        assert!(
            saved_floats
                .iter()
                .all(|&(_, home)| float_homes.contains(&home)),
        );
        for node in [point, call, poll] {
            assert!(
                values
                    .iter()
                    .all(|&value| liveness.is_live_after(node, value))
            );
            assert_eq!(
                allocation.node(node).eager,
                values
                    .iter()
                    .map(|value| allocation.spill[value])
                    .collect::<Vec<_>>(),
            );
        }
        assert!(
            values
                .iter()
                .all(|value| allocation.definition_spills.contains(value)),
            "the clobbering call needs initialized homes for all live values",
        );
        assert!(allocation.node(call).eager_spills.is_empty());
        assert!(allocation.node(call).live_homes.is_empty());
        assert!(
            allocation.node(boxed).moves.iter().any(|movement| {
                movement.from == allocation.spill[&reloaded]
                    && matches!(movement.to, Location::Fp(_))
            }),
            "the floating input must reload from its authoritative untagged home",
        );
        assert!(
            allocation
                .node(poll)
                .live_homes
                .iter()
                .any(|&(register, home)| {
                    matches!(register, Location::Fp(_)) && home == allocation.spill[&reloaded]
                }),
            "the poll must save the reloaded floating register through that same home",
        );
        assert_memory_backedge_restores(&allocation, &floats, entry_control, poll, header);
    }
}

#[test]
fn cold_exit_roots_only_the_selected_finalized_recipe() {
    let mut graph = Graph::default();
    let block = graph.new_block();
    let argument = append(
        &mut graph,
        block,
        Kind::InitialRegister(0),
        &[],
        Repr::Tagged,
    );
    let dead = append(
        &mut graph,
        block,
        Kind::InitialRegister(1),
        &[],
        Repr::Tagged,
    );
    let before = state(&mut graph, &[argument, dead]);
    collecting_reader(&mut graph, block, before);
    let callee = graph.add_node(
        Kind::ConstTagged(otter_vm::value::tag::VALUE_NULL),
        &[],
        Repr::Tagged,
    );
    let eager = state(&mut graph, &[argument, callee]);
    let call = append(
        &mut graph,
        block,
        Kind::CallJs {
            pc: 0,
            plan: crate::call_linkage::CallPlan::Generic,
            construct: false,
            receiver: false,
            allocation: None,
        },
        &[callee, argument],
        Repr::Tagged,
    );
    graph.node_mut(call).eager = Some(eager);
    let lazy = state(&mut graph, &[call]);
    graph.node_mut(call).lazy = Some(lazy);
    terminate(&mut graph, block, Kind::Return, &[call]);

    let allocation = allocate(&graph, &[block], AARCH64);
    let Location::TaggedSlot(argument_home) = allocation.spill[&argument] else {
        panic!("argument needs a tagged canonical home");
    };
    let Location::TaggedSlot(result_home) = allocation.spill[&call] else {
        panic!("lazy result needs a tagged canonical home");
    };
    assert_ne!(
        argument_home, result_home,
        "argument and result overlap at the call"
    );
    assert_eq!(
        allocation.spill[&dead],
        Location::TaggedSlot(result_home),
        "the result reuses the dead occupant's home"
    );
    assert!(
        allocation.definition_spills.contains(&argument),
        "the actual call consumes its initialized memory home"
    );
    let eager_rooted = Allocation::recipe_tagged_homes(&allocation.node(call).eager);
    assert!(eager_rooted.contains(&argument_home));
    assert!(
        !eager_rooted.contains(&result_home),
        "the eager exit owes no unproduced result"
    );
    let lazy_rooted = Allocation::recipe_tagged_homes(&allocation.node(call).lazy);
    assert!(!lazy_rooted.contains(&argument_home));
    assert!(
        lazy_rooted.contains(&result_home),
        "the selected lazy recipe roots its produced result even in a reused slot"
    );
}

#[test]
fn empty_allocation_pressure_keeps_late_output_occupant_and_memory_homes() {
    let mut graph = Graph::default();
    let block = graph.new_block();
    let mut values: Vec<NodeId> = (0..24)
        .map(|index| {
            append(
                &mut graph,
                block,
                Kind::InitialRegister(index),
                &[],
                Repr::Tagged,
            )
        })
        .collect();
    let double = graph.constant(Kind::ConstFloat64(1.5f64.to_bits()));
    for _ in 0..5 {
        values.push(append(
            &mut graph,
            block,
            Kind::Float64Add,
            &[double, double],
            Repr::Float64,
        ));
    }
    let before_object = state(&mut graph, &values);
    let object = append(&mut graph, block, Kind::NewObject, &[], Repr::Tagged);
    graph.node_mut(object).eager = Some(before_object);
    values.push(object);
    let before_array = state(&mut graph, &values);
    let array = append(&mut graph, block, Kind::NewArrayEmpty, &[], Repr::Tagged);
    graph.node_mut(array).eager = Some(before_array);
    for &value in &values[..24] {
        append(&mut graph, block, Kind::CheckNotHole, &[value], Repr::None);
    }
    let null = graph.constant(Kind::ConstTagged(otter_vm::value::tag::VALUE_NULL));
    let final_state = state(&mut graph, &values);
    let collect = append(
        &mut graph,
        block,
        Kind::Instanceof,
        &[object, array],
        Repr::Tagged,
    );
    graph.node_mut(collect).eager = Some(final_state);
    terminate(&mut graph, block, Kind::Return, &[null]);
    let allocation = allocate(&graph, &[block], AARCH64);
    let point = allocation.node(object);
    assert_eq!(point.gp_temps.len(), 4);
    let output = point.result.expect("allocation output");
    assert!(
        !point
            .gp_temps
            .iter()
            .any(|&temp| output == Location::Gp(temp))
    );
    assert!(
        point
            .live_homes
            .iter()
            .any(|&(register, _)| register == output),
        "actual pressure must evict an output-register occupant after live saves were planned"
    );
    assert!(
        point
            .live_homes
            .iter()
            .filter(|&&(register, _)| matches!(register, Location::Fp(_)))
            .count()
            >= 5
    );
    assert!(
        values[..24]
            .iter()
            .any(|value| { allocation.definition_spills.contains(value) }),
        "real memory spills must be established before the cold path"
    );
    let Location::TaggedSlot(object_home) = allocation.spill[&object] else {
        panic!("tagged object home")
    };
    assert!(
        allocation
            .node(array)
            .rooted_tagged_homes
            .as_deref()
            .is_some_and(|rooted| rooted.contains(&object_home)),
        "array collection must root the previously allocated object even if memory-only"
    );
}
