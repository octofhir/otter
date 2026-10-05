//! Executable proofs of the production x86-64 Graph memory dispatch.
//!
//! # Contents
//! - Persistent banks for capacities zero, four and sixty-four under GP pressure.
//! - All indexed representations, index normalization and numeric hole words.
//! - Float64 Uint8Clamp under every MXCSR rounding-control setting.
//!
//! # Invariants
//! - Probes invoke actual Graph emitters, not a parallel instruction model.
//! - Raw fixture storage is owned and never crosses an allocation or GC call.
//! - These probes establish native memory/encoding behavior; moving collection
//!   is covered by runtime tests over the complete published native frame.
//!
//! # See also
//! - [`super`] for the production dispatcher these executable probes exercise.

use dynasmrt::{AssemblyOffset, DynasmApi, DynasmLabelApi, dynasm, x64::Assembler};
use otter_vm::{
    JitCompileSnapshot,
    jit::{JitElementRepr as E, JitHoleBitmap},
    object::FieldLocation,
    value::tag,
};

use super::super::*;

fn snapshot() -> JitCompileSnapshot {
    JitCompileSnapshot::without_feedback(
        771,
        3,
        3,
        vec![otter_vm::jit::JitTestInstruction::new(
            otter_bytecode::Op::ReturnUndefined,
            0,
            0,
            vec![],
        )],
    )
}

fn node(
    graph: &mut Graph,
    allocation: &mut Allocation,
    kind: Kind,
    inputs: &[Location],
    result: Option<Location>,
) -> NodeId {
    let id = graph.add_node(kind, &[], Repr::Tagged);
    allocation
        .nodes
        .push(super::super::super::regalloc::NodeAllocation {
            inputs: inputs.iter().copied().collect(),
            result,
            ..Default::default()
        });
    id
}

fn codegen<'a>(
    view: &'a JitCompileSnapshot,
    graph: &'a Graph,
    allocation: &'a Allocation,
    plan: &'a crate::template::TemplatePlan,
    transitions: &'a crate::entry::TransitionTable,
) -> Codegen<'a> {
    let mut ops = Assembler::new().unwrap();
    let mut label = || ops.new_dynamic_label();
    let returned = label();
    let deopt = label();
    let fatal = label();
    let construct = label();
    let side_exit = label();
    let threw = label();
    let committed_throw = label();
    let propagate = label();
    let materialize = label();
    let slots = SlotLayout::of(allocation).unwrap();
    Codegen {
        ops,
        relocations: RelocationCapture::new(false),
        view,
        inline_views: &[],
        graph,
        allocation,
        layout: &[],
        slots,
        sp_delta: 0,
        labels: FxHashMap::default(),
        exits: vec![],
        deferred: vec![],
        returned,
        deopt,
        fatal,
        activation: crate::frame::ActivationExits {
            construct,
            side_exit,
        },
        spill: crate::frame::SpillArea {
            bytes: slots.bytes(),
            scratch_slot: Some(slots.spill_tagged),
            safepoint: FIRST_SITE_SAFEPOINT,
            saved_pairs: 0,
        },
        transitions,
        deopt_runtime: 0,
        plan,
        plan_index: FxHashMap::default(),
        shared_property: Default::default(),
        no_direct_call_events: None,
        no_code_map: None,
        spliced_functions: Default::default(),
        node_offsets: vec![],
        threw,
        committed_throw,
        propagate,
        materialize,
        sites: metadata::SitePlan::new(slots.spill_tagged),
        return_sites: Vec::new(),
    }
}

#[test]
fn graph_own_fields_keep_both_banks_and_allocatable_registers() {
    let view = snapshot();
    let layout = view.field_layout;
    let plan = crate::template::TemplatePlan::build_unfused(&view).unwrap();
    let transitions = crate::entry::TransitionTable::resolve();
    let mut storage = vec![0x7766_5544_3322_1100_u64; 6200];
    let object = storage.as_mut_ptr() as u64;
    // SAFETY: both slabs and every selected word stay inside this owned span.
    let slab_a = unsafe { storage.as_mut_ptr().add(100) } as u64;
    let slab_b = unsafe { storage.as_mut_ptr().add(180) } as u64;
    assert_eq!(object >> 32, slab_a >> 32);
    assert_eq!(object >> 32, slab_b >> 32);
    let mut canaries = [0_u64; 7];
    for capacity in [0, 4, 64] {
        let mut fields = vec![
            FieldLocation::for_slot(capacity as u32, capacity),
            FieldLocation::overflow(63),
            FieldLocation::overflow(5000),
        ];
        if capacity != 0 {
            fields.push(FieldLocation::for_slot(0, capacity));
            fields.push(FieldLocation::for_slot(capacity as u32 - 1, capacity));
        }
        for field in fields {
            let mut graph = Graph::default();
            let mut allocation = Allocation::default();
            let store = node(
                &mut graph,
                &mut allocation,
                Kind::StoreOwnField(field),
                &[Location::Gp(7), Location::Gp(6)],
                None,
            );
            let load = node(
                &mut graph,
                &mut allocation,
                Kind::LoadOwnField(field),
                &[Location::Gp(7)],
                Some(Location::Gp(0)),
            );
            let mut cg = codegen(&view, &graph, &allocation, &plan, &transitions);
            dynasm!(cg.ops ; .arch x64 ; push rbx ; push r12 ; push rdx
                ; mov rcx, QWORD 0x1111_1111_1111_1111 ; mov rdx, QWORD 0x2222_2222_2222_2222
                ; mov rbx, QWORD 0x3333_3333_3333_3333 ; mov r8, QWORD 0x4444_4444_4444_4444
                ; mov r9, QWORD 0x5555_5555_5555_5555 ; mov r12, QWORD 0x6666_6666_6666_6666
                ; mov r11, QWORD 0x7777_7777_7777_7777 ; movq xmm14, r11);
            assert!(cg.emit_memory(store).unwrap());
            assert!(cg.emit_memory(load).unwrap());
            dynasm!(cg.ops ; .arch x64 ; pop r11
                ; mov [r11], rcx ; mov [r11 + 8], rdx ; mov [r11 + 16], rbx
                ; mov [r11 + 24], r8 ; mov [r11 + 32], r9 ; mov [r11 + 40], r12 ; movq [r11 + 48], xmm14
                ; pop r12 ; pop rbx ; ret);
            let buffer = cg.ops.finalize().unwrap();
            // SAFETY: the native body is a SysV probe over owned cell words;
            // it saves its nonvolatile registers and performs no C/GC calls.
            let probe: unsafe extern "sysv64" fn(u64, u64, *mut u64) -> u64 =
                unsafe { std::mem::transmute(buffer.ptr(AssemblyOffset(0))) };
            let handle = (object + u64::from(layout.slab_handle_byte)) as *mut u32;
            let word = |slab: u64| {
                if field.is_inline() {
                    object + u64::from(layout.inline_byte(field))
                } else {
                    slab + u64::from(layout.slab_words_byte) + u64::from(field.byte_offset())
                }
            };
            // SAFETY: fixture handle and bank words are aligned and in storage.
            unsafe {
                handle.write(slab_a as u32);
            }
            let header_before = storage[0];
            let other_bank = if field.is_inline() {
                slab_a + u64::from(layout.slab_words_byte)
            } else {
                object + u64::from(layout.inline_values_byte)
            } as *const u64;
            let other_before = unsafe { other_bank.read() };
            let expected = tag::NUMBER_TAG | 731;
            assert_eq!(
                unsafe { probe(object, expected, canaries.as_mut_ptr()) },
                expected,
                "capacity {capacity} {field:?}"
            );
            assert_eq!(
                canaries,
                [
                    0x1111_1111_1111_1111,
                    0x2222_2222_2222_2222,
                    0x3333_3333_3333_3333,
                    0x4444_4444_4444_4444,
                    0x5555_5555_5555_5555,
                    0x6666_6666_6666_6666,
                    0x7777_7777_7777_7777
                ]
            );
            assert_eq!(unsafe { (word(slab_a) as *const u64).read() }, expected);
            assert_eq!(storage[0], header_before, "GC header is intact");
            assert_eq!(
                unsafe { other_bank.read() },
                other_before,
                "opposite bank is intact"
            );
            if !field.is_inline() {
                unsafe {
                    handle.write(slab_b as u32);
                }
                assert_eq!(
                    unsafe { probe(object, expected + 1, canaries.as_mut_ptr()) },
                    expected + 1
                );
                assert_eq!(unsafe { (word(slab_b) as *const u64).read() }, expected + 1);
                assert_eq!(
                    unsafe { (word(slab_a) as *const u64).read() },
                    expected,
                    "live suffix handle reread"
                );
            }
        }
    }
}

#[test]
fn graph_indexed_memory_executes_every_current_representation() {
    let view = snapshot();
    let plan = crate::template::TemplatePlan::build_unfused(&view).unwrap();
    let transitions = crate::entry::TransitionTable::resolve();
    for element in [
        E::Boxed,
        E::Int8,
        E::Uint8,
        E::Uint8Clamped,
        E::Int16,
        E::Uint16,
        E::Int32,
        E::Uint32,
        E::Float32,
        E::Float64,
    ] {
        let floating = matches!(element, E::Float32 | E::Float64);
        let mut graph = Graph::default();
        let mut allocation = Allocation::default();
        let value = if floating {
            Location::Fp(0)
        } else {
            Location::Gp(2)
        };
        let store = node(
            &mut graph,
            &mut allocation,
            Kind::StoreElement(element),
            &[Location::Gp(7), Location::Gp(6), value],
            None,
        );
        let load = node(
            &mut graph,
            &mut allocation,
            if matches!(element, E::Uint32) {
                Kind::LoadElementUint32ToFloat64
            } else {
                Kind::LoadElement(element)
            },
            &[Location::Gp(7), Location::Gp(6)],
            Some(if floating || matches!(element, E::Uint32) {
                Location::Fp(1)
            } else {
                Location::Gp(0)
            }),
        );
        let mut cg = codegen(&view, &graph, &allocation, &plan, &transitions);
        assert!(cg.emit_memory(store).unwrap());
        assert!(cg.emit_memory(load).unwrap());
        if floating || matches!(element, E::Uint32) {
            dynasm!(cg.ops ; .arch x64 ; movsd xmm0, xmm1);
        }
        dynasm!(cg.ops ; .arch x64 ; ret);
        for exit in &cg.exits {
            let label = exit.label;
            dynasm!(cg.ops ; .arch x64 ; =>label ; ud2);
        }
        let buffer = cg.ops.finalize().unwrap();
        let mut words = [0xaaaa_aaaa_aaaa_aaaa_u64; 4];
        let base = words.as_mut_ptr() as u64;
        let poisoned_index = 0xfedc_ba98_0000_0001;
        // SAFETY: every generated access is at index one of owned storage;
        // upper index bits are intentionally poison and must be discarded.
        unsafe {
            if floating {
                let probe: unsafe extern "sysv64" fn(u64, u64, f64) -> f64 =
                    std::mem::transmute(buffer.ptr(AssemblyOffset(0)));
                let input = 1.234567890123;
                let expected = if matches!(element, E::Float32) {
                    f64::from(input as f32)
                } else {
                    input
                };
                assert_eq!(probe(base, poisoned_index, input), expected);
            } else if matches!(element, E::Uint32) {
                let probe: unsafe extern "sysv64" fn(u64, u64, u64) -> f64 =
                    std::mem::transmute(buffer.ptr(AssemblyOffset(0)));
                assert_eq!(probe(base, poisoned_index, 0xffff_ffff), 4294967295.0);
            } else {
                let probe: unsafe extern "sysv64" fn(u64, u64, u64) -> u64 =
                    std::mem::transmute(buffer.ptr(AssemblyOffset(0)));
                let (input, expected) = match element {
                    E::Boxed => (tag::NUMBER_TAG | 731, tag::NUMBER_TAG | 731),
                    E::Int8 => (0xff, u64::from((-1_i32) as u32)),
                    E::Uint8 => (0xff, 255),
                    E::Uint8Clamped => (731, 255),
                    E::Int16 => (0xffff, u64::from((-1_i32) as u32)),
                    E::Uint16 => (0xffff, 65535),
                    E::Int32 => (0xffff_ffff, 0xffff_ffff),
                    _ => unreachable!(),
                };
                assert_eq!(probe(base, poisoned_index, input), expected, "{element:?}");
            }
        }
        assert_eq!(
            words[3], 0xaaaa_aaaa_aaaa_aaaa,
            "unselected storage is intact"
        );
    }
}

#[test]
fn graph_uint8_clamp_uses_ties_to_even_independently_of_mxcsr() {
    let view = snapshot();
    let plan = crate::template::TemplatePlan::build_unfused(&view).unwrap();
    let transitions = crate::entry::TransitionTable::resolve();
    let mut graph = Graph::default();
    let mut allocation = Allocation::default();
    let store = node(
        &mut graph,
        &mut allocation,
        Kind::StoreElement(E::Uint8Clamped),
        &[Location::Gp(7), Location::Gp(6), Location::Fp(0)],
        None,
    );
    let load = node(
        &mut graph,
        &mut allocation,
        Kind::LoadElement(E::Uint8Clamped),
        &[Location::Gp(7), Location::Gp(6)],
        Some(Location::Gp(0)),
    );
    let mut cg = codegen(&view, &graph, &allocation, &plan, &transitions);
    dynasm!(cg.ops ; .arch x64 ; sub rsp, 16 ; stmxcsr [rsp]
        ; mov r10d, [rsp] ; and r10d, !0x6000 ; or r10d, edx ; mov [rsp + 4], r10d ; ldmxcsr [rsp + 4]);
    cg.sp_delta = 16;
    assert!(cg.emit_memory(store).unwrap());
    assert!(cg.emit_memory(load).unwrap());
    assert_eq!(cg.sp_delta, 16, "temporary clamp packet was retired");
    dynasm!(cg.ops ; .arch x64 ; ldmxcsr [rsp] ; add rsp, 16 ; ret);
    let buffer = cg.ops.finalize().unwrap();
    // SAFETY: owned bytes, a noncollecting SysV probe, and MXCSR restored on
    // every return preserve the caller's control environment.
    let probe: unsafe extern "sysv64" fn(u64, u64, f64, u32) -> u64 =
        unsafe { std::mem::transmute(buffer.ptr(AssemblyOffset(0))) };
    let mut bytes = [0x77_u8; 4];
    for mode in [0, 0x2000, 0x4000, 0x6000] {
        for (input, expected) in [
            (f64::NAN, 0),
            (f64::NEG_INFINITY, 0),
            (-1.0, 0),
            (-0.0, 0),
            (0.5, 0),
            (1.5, 2),
            (2.5, 2),
            (253.5, 254),
            (254.5, 254),
            (255.0, 255),
            (f64::INFINITY, 255),
        ] {
            assert_eq!(
                unsafe { probe(bytes.as_mut_ptr() as u64, 1, input, mode) },
                expected,
                "{input} mode {mode}"
            );
            assert_eq!([bytes[0], bytes[2], bytes[3]], [0x77; 3]);
        }
    }
}

#[test]
fn graph_numeric_holes_use_the_indexed_bitmap_word() {
    let view = snapshot();
    let plan = crate::template::TemplatePlan::build_unfused(&view).unwrap();
    let transitions = crate::entry::TransitionTable::resolve();
    let mut graph = Graph::default();
    let mut allocation = Allocation::default();
    let guard = node(
        &mut graph,
        &mut allocation,
        Kind::CheckHoleyElementPresent(JitHoleBitmap {
            capacity_byte: -8,
            kind_byte: 0,
            packed_kind: 0,
            holey_kind: 1,
        }),
        &[Location::Gp(7), Location::Gp(6)],
        None,
    );
    let mut cg = codegen(&view, &graph, &allocation, &plan, &transitions);
    assert!(cg.emit_guard(guard).unwrap());
    dynasm!(cg.ops ; .arch x64 ; mov eax, 1 ; ret);
    for exit in &cg.exits {
        let label = exit.label;
        dynasm!(cg.ops ; .arch x64 ; =>label ; xor eax, eax ; ret);
    }
    let buffer = cg.ops.finalize().unwrap();
    let mut words = [0_u64; 131];
    words[0] = 128;
    words[129] = 1 << 63;
    words[130] = 1 << 1;
    // SAFETY: capacity word, elements and two bitmap words are owned. The
    // generated guard reads only the bitmap selected by the normalized index.
    let probe: unsafe extern "sysv64" fn(u64, u64) -> u64 =
        unsafe { std::mem::transmute(buffer.ptr(AssemblyOffset(0))) };
    let base = unsafe { words.as_mut_ptr().add(1) } as u64;
    for (index, expected) in [(62, 1), (63, 0), (64, 1), (65, 0), (66, 1)] {
        assert_eq!(
            unsafe { probe(base, 0x1234_5678_0000_0000 | index) },
            expected
        );
    }
}

#[test]
fn graph_bare_function_guard_populates_the_current_identity_cell() {
    let view = snapshot();
    let plan = crate::template::TemplatePlan::build_unfused(&view).unwrap();
    let transitions = crate::entry::TransitionTable::resolve();
    let mut cell = 0_u64;
    let address = std::ptr::from_mut(&mut cell) as u64;
    let mut graph = Graph::default();
    let mut allocation = Allocation::default();
    let guard = node(
        &mut graph,
        &mut allocation,
        Kind::CheckFunction {
            function_id: 37,
            cell: address,
        },
        &[Location::Gp(7)],
        None,
    );
    let mut cg = codegen(&view, &graph, &allocation, &plan, &transitions);
    assert!(cg.emit_guard(guard).unwrap());
    cg.load_immediate(11, address);
    dynasm!(cg.ops ; .arch x64 ; mov rax, [r11] ; ret);
    for exit in &cg.exits {
        let label = exit.label;
        dynasm!(cg.ops ; .arch x64 ; =>label ; xor eax, eax ; ret);
    }
    let buffer = cg.ops.finalize().unwrap();
    // SAFETY: the identity cell is address-stable owned storage until the
    // probe is dropped, and this guard performs no allocation or native call.
    let probe: unsafe extern "sysv64" fn(u64) -> u64 =
        unsafe { std::mem::transmute(buffer.ptr(AssemblyOffset(0))) };
    let expected = tag::box_function_id(37);
    assert_eq!(unsafe { probe(expected) }, expected);
    assert_eq!(
        cell, expected,
        "bare identity has populated the current cache"
    );
    assert_eq!(unsafe { probe(expected) }, expected, "cache hit");
    assert_eq!(
        unsafe { probe(tag::box_function_id(38)) },
        0,
        "wrong callee misses"
    );
    assert_eq!(cell, expected, "miss never changes the identity cell");
}
