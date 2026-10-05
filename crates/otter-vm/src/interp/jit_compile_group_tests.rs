//! Exact source snapshot allocation-group policy admission.
//!
//! # Contents
//! - Real linked source and the production literal plan baker.
//! - Normal admission, GC stress and bootstrap-tenuring refusal.
//!
//! # Invariants
//! The source snapshot carries the one heap policy observed before baking;
//! absent preparation is false. Standalone shell geometry remains prepared
//! even when grouping is refused. No environment variable or test-name policy
//! is consulted by either producer or consumer.
//!
//! # See also
//! - `GcHeap::machine_allocation_allowed` is the sole runtime policy owner.
//! - Graph group tests prove exact source-view use and emitted standalone code.

use super::*;
use otter_bytecode::{FunctionCodeBuilder, Operand};

#[test]
fn production_literal_snapshot_freezes_current_heap_group_capability() {
    let mut vm = Interpreter::new().expect("fixture runtime bootstrap");
    let mut module = crate::test_support::minimal_bytecode_module("group-policy.js");
    module.functions[0].locals = 2;
    let mut code = FunctionCodeBuilder::new();
    code.push(Op::NewObject, &[Operand::Register(0)]);
    code.push(
        Op::NewArray,
        &[Operand::Register(1), Operand::ConstIndex(0)],
    );
    code.push(Op::ReturnValue, &[Operand::Register(0)]);
    module.functions[0].code = code.finish();
    let context = vm
        .link_module(module, crate::source_registry::SourceRegistry::default())
        .expect("verified consecutive fixed shells");
    let fid = context.function_base();
    for (stride, tenure, expected) in [
        (0, false, true),
        (1, false, false),
        (0, true, false),
        (0, false, true),
    ] {
        vm.gc_heap.set_gc_stress(stride, true);
        vm.gc_heap.set_tenure_all(tenure);
        let mut view = context
            .jit_compile_snapshot(fid)
            .expect("exact linked source snapshot");
        assert!(
            !view.literal_allocations.group_allowed,
            "unprepared snapshots cannot group"
        );
        assert_eq!(vm.gc_heap.machine_allocation_allowed(), expected);
        vm.bake_literal_allocations(&mut view, &context, fid)
            .expect("real literal plan preparation");
        assert_eq!(view.literal_allocations.group_allowed, expected);
        assert_eq!(view.literal_allocations.realm_id, 0);
        assert!(
            view.literal_allocations.object.is_some(),
            "disabled grouping retains native standalone object geometry"
        );
        assert!(view.literal_allocations.array.cell_bytes > 0);
    }
}
