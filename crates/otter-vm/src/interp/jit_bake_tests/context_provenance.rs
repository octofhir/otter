//! Captured-slot baking over distinct context register origins.
//!
//! # Contents
//! - Fresh local writes versus captured reads at the same coordinate.
//! - Parent, alias, redefinition and implicit-entry writes that prevent baking.
//!
//! # Invariants
//! Fixtures pass ordinary bytecode verification and use a live watched context.
//! An admitted constant retains the exact context and install-time value proof.

use super::*;
use otter_bytecode::{
    ContextCoord, ScopeDescriptor, ScopeFlags, ScopeKind, SlotDescriptor, SlotKind,
};

fn reg(index: u16) -> Operand {
    Operand::Register(index)
}

fn coord(depth: u16, slot: u16) -> Operand {
    Operand::Imm32(ContextCoord::new(depth, slot).unwrap().to_imm32())
}

fn function(operations: Vec<(Op, Vec<Operand>)>) -> Function {
    Function {
        name: "context-provenance".into(),
        locals: 6,
        param_count: 1,
        length: 1,
        scopes: vec![ScopeDescriptor {
            kind: ScopeKind::Body,
            flags: ScopeFlags::default(),
            slots: vec![SlotDescriptor {
                name: "local".into(),
                kind: SlotKind::Let,
                exported: false,
            }],
        }],
        code: operations
            .into_iter()
            .enumerate()
            .map(|(pc, (op, operands))| Instruction {
                pc: pc as u32,
                op,
                operands,
            })
            .collect::<Vec<_>>()
            .into(),
        ..Function::default()
    }
}

fn captured_read(mut operations: Vec<(Op, Vec<Operand>)>) -> Function {
    operations.extend([
        (Op::LoadContextSlot, vec![reg(3), reg(2), coord(0, 0)]),
        (Op::ReturnValue, vec![reg(3)]),
    ]);
    function(operations)
}

fn fresh_context() -> Vec<(Op, Vec<Operand>)> {
    vec![
        (Op::LoadClosureContext, vec![reg(2)]),
        (Op::CreateContext, vec![reg(1), reg(2), Operand::Imm32(0)]),
    ]
}

fn assert_baked(function: Function, expected: bool, label: &str) {
    let mut vm = Interpreter::new().expect("fixture interpreter bootstrap");
    let mut module = crate::test_support::minimal_bytecode_module("context-provenance.js");
    module.functions = vec![function];
    let context = vm
        .link_module(module, crate::source_registry::SourceRegistry::default())
        .expect("verified context fixture");
    vm.with_handle_scope(|vm, _scope| {
        let outer = crate::context::alloc_context_with_roots(
            &mut vm.gc_heap,
            crate::context::ContextShape {
                scope_function_id: 0,
                scope_index: 0,
                slot_count: 1,
                has_extension: false,
            },
            Value::undefined(),
            |_| false,
            &mut |_| {},
        )
        .unwrap();
        assert!(crate::context::write_slot(
            &mut vm.gc_heap,
            outer,
            0,
            Value::number_i32(317)
        ));
        let watch = vm
            .jit_code_registry
            .ensure_context_watch(&mut vm.gc_heap, outer)
            .expect("watchable captured context");
        let env = crate::interp::jit_bake_env::BakeEnv::capture(vm);
        let mut baker = super::super::Baker::new(&env, None);
        let constants =
            baker.bake_closure_constants(Value::context(outer), context.exec_function(0).unwrap());
        assert_eq!(constants.len(), usize::from(expected), "{label}");
        assert_eq!(
            baker.install.context_slot_watches.len(),
            constants.len(),
            "only admitted constants retain an install watch: {label}"
        );
        if expected {
            let constant = &constants[0];
            assert_eq!(
                (constant.depth, constant.slot, constant.watch),
                (0, 0, watch)
            );
            assert!(matches!(
                constant.value,
                crate::jit::JitConstantValue::Int32(317)
            ));
            let proof = &baker.install.context_slot_watches[0];
            assert_eq!(proof.context.held(), Value::context(outer));
            assert_eq!((proof.slot, proof.watch), (0, watch));
            assert_eq!(proof.bits, Value::number_i32(317).to_bits());
            assert!(proof.value_cell.is_none());
        }
    });
}

#[test]
fn fresh_local_context_writes_preserve_captured_slot_constants() {
    for store in [Op::StoreContextSlot, Op::StoreContextSlotChecked] {
        let mut operations = fresh_context();
        operations.push((store, vec![reg(0), reg(1), coord(0, 0)]));
        assert_baked(
            captured_read(operations),
            true,
            "distinct fresh own context",
        );
    }
}

#[test]
fn fresh_context_reads_do_not_create_captured_constants() {
    let mut operations = fresh_context();
    operations.extend([
        (Op::LoadContextSlot, vec![reg(3), reg(1), coord(0, 0)]),
        (Op::ReturnValue, vec![reg(3)]),
    ]);
    assert_baked(
        function(operations),
        false,
        "the read is not from the closure",
    );
}

#[test]
fn initially_undefined_fresh_register_cannot_alias_a_captured_context() {
    assert_baked(
        captured_read(vec![
            (Op::LoadClosureContext, vec![reg(2)]),
            (Op::StoreContextSlot, vec![reg(0), reg(1), coord(0, 0)]),
            (Op::CreateContext, vec![reg(1), reg(2), Operand::Imm32(0)]),
        ]),
        true,
        "the uninitialized local is undefined and cannot complete a context write",
    );
}

#[test]
fn captured_parent_and_unknown_writes_prevent_slot_baking() {
    let cases = [
        (
            "captured write",
            vec![(Op::StoreContextSlot, vec![reg(0), reg(2), coord(0, 0)])],
        ),
        (
            "fresh parent write",
            vec![(Op::StoreContextSlot, vec![reg(0), reg(1), coord(1, 0)])],
        ),
        (
            "moved captured alias",
            vec![
                (Op::LoadLocal, vec![reg(1), Operand::Imm32(2)]),
                (Op::StoreContextSlot, vec![reg(0), reg(1), coord(0, 0)]),
            ],
        ),
        (
            "unknown parent coordinate",
            vec![
                (Op::LoadLocal, vec![reg(1), Operand::Imm32(0)]),
                (Op::StoreContextSlot, vec![reg(0), reg(1), coord(7, 0)]),
            ],
        ),
        (
            "local-index redefinition",
            vec![
                (Op::StoreLocal, vec![reg(2), Operand::Imm32(1)]),
                (Op::StoreContextSlot, vec![reg(0), reg(1), coord(7, 0)]),
            ],
        ),
        (
            "second register of a pair definition",
            vec![
                (Op::GetIterator, vec![reg(0), reg(4)]),
                (Op::StoreContextSlot, vec![reg(0), reg(1), coord(0, 0)]),
            ],
        ),
        (
            "receiver load redefinition",
            vec![
                (Op::LoadThis, vec![reg(1)]),
                (Op::StoreContextSlot, vec![reg(0), reg(1), coord(7, 0)]),
            ],
        ),
        (
            "branch-dependent definition",
            vec![
                (Op::JumpIfFalse, vec![Operand::Imm32(1), reg(0)]),
                (Op::LoadClosureContext, vec![reg(1)]),
                (Op::StoreContextSlot, vec![reg(0), reg(1), coord(7, 0)]),
            ],
        ),
    ];
    for (label, tail) in cases {
        let mut operations = fresh_context();
        operations.extend(tail);
        assert_baked(captured_read(operations), false, label);
    }
}

#[test]
fn implicit_parameter_and_handler_definitions_are_not_fresh() {
    let mut parameter = fresh_context();
    parameter.push((Op::StoreContextSlot, vec![reg(0), reg(1), coord(7, 0)]));
    let mut parameter = captured_read(parameter);
    parameter.param_count = 2;
    parameter.length = 2;
    assert_baked(parameter, false, "parameter entry may carry a context");

    let mut handler = fresh_context();
    handler.push((Op::StoreContextSlot, vec![reg(0), reg(1), coord(7, 0)]));
    let mut handler = captured_read(handler);
    handler.handlers = vec![otter_bytecode::ExceptionHandler {
        start: 1,
        end: 2,
        target: 2,
        exception: 1,
    }];
    assert_baked(
        handler,
        false,
        "handler entry overwrites the context register",
    );
}

fn copied_context_fixture(operations: Vec<(Op, Vec<Operand>)>) -> Function {
    let mut function = captured_read(operations);
    // Scope zero retains the captured fixture's one-slot shape. The fresh
    // local scope has two slots and writes only coordinate one.
    function.scopes.push(ScopeDescriptor {
        kind: ScopeKind::Block,
        flags: ScopeFlags::default(),
        slots: ["unused", "iteration"]
            .into_iter()
            .map(|name| SlotDescriptor {
                name: name.into(),
                kind: SlotKind::Let,
                exported: false,
            })
            .collect(),
    });
    function
}

#[test]
fn copied_context_depth_zero_writes_preserve_other_captured_coordinates() {
    for store in [Op::StoreContextSlot, Op::StoreContextSlotChecked] {
        assert_baked(
            copied_context_fixture(vec![
                (Op::LoadClosureContext, vec![reg(2)]),
                (Op::CreateContext, vec![reg(1), reg(2), Operand::Imm32(1)]),
                (Op::StoreContextSlot, vec![reg(0), reg(1), coord(0, 1)]),
                (Op::CopyContext, vec![reg(1), reg(1)]),
                (store, vec![reg(0), reg(1), coord(0, 1)]),
            ]),
            true,
            "fresh per-iteration slot one is disjoint from captured slot zero",
        );
    }
}

#[test]
fn copied_context_parent_and_mixed_writes_remain_conservative() {
    for (label, redefine, depth) in [
        ("copied parent may alias captured storage", false, 1),
        ("copied register also has a captured definition", true, 0),
    ] {
        let mut operations = vec![
            (Op::LoadClosureContext, vec![reg(2)]),
            (Op::CreateContext, vec![reg(1), reg(2), Operand::Imm32(1)]),
            (Op::CopyContext, vec![reg(1), reg(1)]),
        ];
        if redefine {
            operations.push((Op::LoadClosureContext, vec![reg(1)]));
        }
        operations.push((Op::StoreContextSlot, vec![reg(0), reg(1), coord(depth, 0)]));
        assert_baked(copied_context_fixture(operations), false, label);
    }
}
