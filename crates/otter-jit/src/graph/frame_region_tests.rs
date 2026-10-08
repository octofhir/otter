//! Native frame geometry rejected before machine code or root tables publish.
//!
//! # Contents
//! - Emitter declines for tagged-root indices and signed deopt slot offsets.
//! - Count overflow and a supported small frame's exact root descriptor.
//! - Two-instruction guard exits over register recipes, and rooted
//!   exception handoff.
//!
//! # Invariants
//! Tests build a real bytecode graph and allocation, then vary only their
//! final region counts. Rejected sizes reach the emitter without allocating
//! the enormous slot regions; the supported buffer is inspected, never run.
//!
//! # See also
//! - [`super::emit`] and [`super::SlotLayout`].

use otter_bytecode::Op;
use otter_vm::jit::JitTestInstruction;

use super::*;

fn emit_region(tagged: u32, untagged: u32) -> Result<Emission, Unsupported> {
    let mut view = JitCompileSnapshot::without_feedback(
        90,
        0,
        1,
        vec![JitTestInstruction::new(Op::ReturnUndefined, 0, 0, vec![])],
    );
    view.object_shape_byte = 8;
    let analysis = std::rc::Rc::new(
        super::super::bytecode::Analysis::build(&view).expect("bytecode analysis"),
    );
    let plan = crate::template::TemplatePlan::build_unfused(&view).expect("baseline plan");
    let baseline = super::super::BaselineSupport::of(&plan, &view);
    let built = super::super::builder::build(&view, &analysis, &baseline, None)
        .expect("graph construction");
    let mut allocation = super::super::regalloc::allocate(
        &built.graph,
        &built.layout,
        super::super::registers::AARCH64,
    );
    allocation.tagged_slots = tagged;
    allocation.untagged_slots = untagged;
    let slots = SlotLayout::of(&allocation)?;
    emit(
        &view,
        &built,
        &allocation,
        &crate::entry::TransitionTable::resolve(),
        7001,
        std::ptr::null(),
        &plan,
        slots,
        true,
        false,
    )
}

fn assert_region_declined(tagged: u32, untagged: u32) {
    match emit_region(tagged, untagged) {
        Err(Unsupported::OperandShape(reason)) => {
            assert_eq!(reason, "graph spill region exceeds frame metadata");
        }
        Err(other) => panic!("region must decline before backend emission: {other:?}"),
        Ok(_) => panic!("unencodable frame region emitted a code object"),
    }
}

#[test]
fn exception_scratch_crossing_tagged_region_limit_declines_before_root_emission() {
    assert_region_declined(u32::from(u16::MAX), 0);
}

#[test]
fn signed_deopt_offset_boundary_declines_before_slot_offset_wrap() {
    // Adding the exception home alone crosses the signed stack-slot limit.
    // The u32 byte count still fits, so an unsigned-only guard misses this.
    assert_region_declined(0, i32::MAX as u32 / 8);
}

#[test]
fn overflowing_slot_word_count_declines_before_frame_emission() {
    assert_region_declined(0, u32::MAX);
}

#[test]
fn overflowing_exception_home_count_declines_during_geometry_planning() {
    assert_region_declined(u32::MAX, 0);
}

#[test]
fn supported_small_frame_emits_only_the_initialized_tagged_region() {
    let emission = emit_region(0, 2).expect("small supported frame");
    assert!(emission.buffer.len() < 16 * 1024);
    assert_eq!(emission.site_records.len(), 1);
    let roots = &emission.site_records[0].spill_roots;
    assert_eq!(
        roots.iter().collect::<Vec<_>>(),
        vec![0],
        "only the exception home is tagged"
    );
}

#[test]
fn guard_exit_reads_registers_in_place_and_roots_pure_throws_in_the_scratch() {
    use super::super::ir::{FrameState, Repr};

    let mut view = JitCompileSnapshot::without_feedback(
        90,
        0,
        2,
        vec![JitTestInstruction::new(Op::ReturnUndefined, 0, 0, vec![])],
    );
    view.object_shape_byte = 8;
    let plan = crate::template::TemplatePlan::build_unfused(&view).expect("baseline plan");
    let mut graph = Graph::default();
    let block = graph.new_block();
    let append = |graph: &mut Graph, kind, inputs: &[NodeId], repr| {
        let node = graph.add_node(kind, inputs, repr);
        graph.node_mut(node).block = Some(block);
        graph.block_mut(block).body.push(node);
        node
    };
    let forgotten = append(&mut graph, Kind::InitialRegister(0), &[], Repr::Tagged);
    let kept = append(&mut graph, Kind::InitialRegister(1), &[], Repr::Tagged);
    let first_state = graph.add_frame_state(FrameState {
        function_id: 90,
        pc: 0,
        byte_pc: 0,
        register_count: 2,
        registers: vec![(0, forgotten), (1, kept)],
        caller: None,
    });
    let first = append(&mut graph, Kind::CheckNotHole, &[forgotten], Repr::None);
    graph.node_mut(first).eager = Some(first_state);
    let second_state = graph.add_frame_state(FrameState {
        function_id: 90,
        pc: 0,
        byte_pc: 0,
        register_count: 2,
        registers: vec![(1, kept)],
        caller: None,
    });
    let second = append(&mut graph, Kind::CheckNotHole, &[kept], Repr::None);
    graph.node_mut(second).eager = Some(second_state);
    let returned = graph.add_node(Kind::Return, &[kept], Repr::None);
    graph.node_mut(returned).block = Some(block);
    graph.block_mut(block).control = Some(returned);
    let built = Built {
        graph,
        layout: vec![block],
        osr_entry: None,
        loop_headers: Vec::new(),
        inline_views: Vec::new(),
    };
    let allocation = super::super::regalloc::allocate(
        &built.graph,
        &built.layout,
        super::super::registers::AARCH64,
    );
    let slots = SlotLayout::of(&allocation).expect("small frame geometry");
    assert_eq!(
        allocation.tagged_slots, 0,
        "guard recipes give no value a home"
    );
    assert!(allocation.node(second).eager_spills.is_empty());
    let [Location::Gp(kept_register)] = allocation.node(second).eager[..] else {
        panic!("the guard's recipe reads the kept value in its register");
    };
    let frames = super::super::frame::deopt_frames(
        &built.graph,
        slots,
        second_state,
        &allocation.node(second).eager,
    );
    assert_eq!(
        frames[0].slots[0].1.location,
        otter_vm::deopt::DeoptLocation::Register(kept_register)
    );
    let emission = emit(
        &view,
        &built,
        &allocation,
        &crate::entry::TransitionTable::resolve(),
        7002,
        std::ptr::null(),
        &plan,
        slots,
        false,
        false,
    )
    .expect("cold exit emission");
    let words: Vec<u32> = emission
        .buffer
        .chunks_exact(4)
        .map(|word| u32::from_le_bytes(word.try_into().expect("ARM word")))
        .collect();
    let store = |register: u8, home: Location| {
        0xf90003e0 | ((slots.offset(home) / 8) << 10) | u32::from(register)
    };
    let exit_index = emission
        .exits
        .iter()
        .position(|exit| exit.node == second)
        .expect("second eager exit") as u32;
    let select = 0x52800000 | (exit_index << 5) | 17; // MOVZ W17, exit index.
    let at = words
        .iter()
        .position(|&word| word == select)
        .expect("the exit selects its recipe");
    assert_eq!(
        words[at + 1] & 0xfc00_0000,
        0x1400_0000,
        "the exit branches straight to the shared handler"
    );
    assert_ne!(
        words[at - 1] & 0xffc0_0000,
        0xf900_0000,
        "a guard's exit stores nothing before selecting its recipe"
    );
    let call_site = otter_vm::native_abi::NATIVE_FRAME_CALL_SITE_OFFSET;
    let throw_sequence = [
        store(0, slots.exception_scratch()),
        // MOVN W16, #1: the entry record, which roots only the scratch.
        0x12800030,
        // STR W16, [X21, #call_site]: published before routing may collect.
        0xb9000000 | ((call_site / 4) << 10) | (21 << 5) | 16,
        0xaa0003e1, // MOV X1, X0: pure exception argument.
        0xaa1403e0, // MOV X0, X20: routing context.
    ];
    assert!(
        words
            .windows(throw_sequence.len())
            .any(|sequence| sequence == throw_sequence),
        "a pure heap exception reaches the traced scratch before routing"
    );
    assert_eq!(
        emission.site_records[0]
            .spill_roots
            .iter()
            .last()
            .expect("scratch root"),
        allocation.tagged_slots as u16
    );
}
