//! Literal-cell roots across nursery movement, promotion and source eviction.
//!
//! # Contents
//! - Exact literal-slot membership in the production All and Young root walks.
//! - Stable native cell addresses through cache growth and moving collection.
//! - Mixed-generation dynamic-source removal followed by new source allocation.
//!
//! # Invariants
//! - Fixtures materialize real String and BigInt values through bytecode helpers.
//! - Generation assertions read current rooted handles, never saved stale values.
//! - Full collections retain all literals, including those omitted from minors.
//!
//! # See also
//! - `crate::runtime_state` owns the production root walk.
//! - `crate::interp::jit_bake_tests` proves emitted literal relocation ownership.

use super::*;
use otter_gc::{ExtraRootSource, RootSet};
use std::collections::{BTreeMap, BTreeSet};

type Key = (usize, u32);

fn string(text: &str) -> Constant {
    Constant::String {
        utf16: text.encode_utf16().collect(),
    }
}

fn bigint(decimal: &str) -> Constant {
    Constant::BigInt {
        decimal: decimal.to_owned(),
    }
}

fn module(name: &str, constants: Vec<Constant>, loads: usize) -> BytecodeModule {
    let mut code = Vec::new();
    for (index, constant) in constants.iter().take(loads).enumerate() {
        code.push(Instruction {
            pc: index as u32,
            op: match constant {
                Constant::String { .. } => Op::LoadString,
                Constant::BigInt { .. } => Op::LoadBigInt,
                _ => unreachable!("fixture contains only primitive literals"),
            },
            operands: vec![Operand::Register(0), Operand::ConstIndex(index as u32)],
        });
    }
    let mut module = module_with(code, 1);
    module.module = name.to_owned();
    module.constants = constants;
    module
}

fn value(vm: &Interpreter, key: Key) -> Value {
    *vm.literal_cells().get(&key).expect("materialized literal")
}

fn young(vm: &Interpreter, key: Key) -> bool {
    let raw = value(vm, key).as_raw_gc().expect("primitive heap cell");
    // SAFETY: the value was freshly read from a traced literal cell, and
    // neither this helper nor the caller allocates before this header read.
    unsafe { (*raw.as_header_ptr()).is_young() }
}

fn addresses(vm: &Interpreter) -> BTreeMap<Key, usize> {
    vm.literal_cells()
        .iter()
        .map(|(key, cell)| (*key, std::ptr::from_ref::<Value>(cell) as usize))
        .collect()
}

fn literal_root_count(vm: &Interpreter, roots: RootSet) -> usize {
    let mut literal_slots = BTreeSet::new();
    for cell in vm.literal_cells().values() {
        cell.trace_value_slots(&mut |slot| {
            assert!(literal_slots.insert(slot as usize));
        });
    }
    let mut visited = BTreeSet::new();
    crate::runtime_state::RuntimeState::new(vm).trace_roots(roots, &mut |slot| {
        if literal_slots.contains(&(slot as usize)) {
            assert!(visited.insert(slot as usize), "one visit per literal slot");
        }
    });
    visited.len()
}

fn prepare(vm: &Interpreter) {
    vm.prepare_collection(vm.gc_heap());
}

fn assert_contents(vm: &Interpreter, context: &ExecutionContext, constants: &[Constant]) {
    for (index, constant) in constants.iter().enumerate() {
        let value = value(vm, context.constant_cache_key(index as u32));
        match constant {
            Constant::String { utf16 } => assert_eq!(
                value
                    .as_string(vm.gc_heap())
                    .expect("String literal")
                    .to_utf16_vec(vm.gc_heap()),
                *utf16
            ),
            Constant::BigInt { decimal } => assert_eq!(
                value
                    .as_big_int()
                    .expect("BigInt literal")
                    .to_decimal_string(vm.gc_heap()),
                *decimal
            ),
            _ => unreachable!(),
        }
    }
}

#[test]
fn promoted_literal_cells_leave_young_roots_without_losing_stable_homes() {
    let mut vm = Interpreter::new().expect("literal root runtime");
    let anchor_constants = vec![string("stable literal anchor"), bigint("9007199254740993")];
    let anchor = vm
        .link_module(
            module("literal-anchor.js", anchor_constants.clone(), 2),
            crate::source_registry::SourceRegistry::default(),
        )
        .unwrap();
    vm.run(&anchor)
        .expect("materialize anchor through bytecode");
    let anchor_addresses = addresses(&vm);
    assert_eq!(anchor_addresses.len(), 2);

    let growth_constants = (0..64)
        .map(|index| {
            if index % 2 == 0 {
                string(&format!("literal cache growth {index}"))
            } else {
                bigint(&(1_000_000 + index).to_string())
            }
        })
        .collect::<Vec<_>>();
    let growth = vm
        .link_module(
            module("literal-growth.js", growth_constants.clone(), 64),
            crate::source_registry::SourceRegistry::default(),
        )
        .unwrap();
    vm.run(&growth).expect("grow the literal map");
    let grown_addresses = addresses(&vm);
    assert_eq!(grown_addresses.len(), 66);
    for (key, address) in &anchor_addresses {
        assert_eq!(grown_addresses[key], *address);
    }
    vm.collect_minor_tracing_runtime_roots();
    vm.collect_minor_tracing_runtime_roots();
    assert!(
        grown_addresses.keys().all(|key| !young(&vm, *key)),
        "two survivals promote every retained literal"
    );
    prepare(&vm);
    assert_eq!(literal_root_count(&vm, RootSet::All), 66);
    let mut observed_young = vec![literal_root_count(&vm, RootSet::Young)];
    let mut expected_young = vec![0];
    let mut fresh_contexts = Vec::new();

    for (name, constant) in [
        ("fresh-string.js", string("fresh moving literal")),
        ("fresh-bigint.js", bigint("123456789012345678901234567890")),
    ] {
        let context = vm
            .link_module(
                module(name, vec![constant.clone()], 1),
                crate::source_registry::SourceRegistry::default(),
            )
            .unwrap();
        vm.run(&context).expect("materialize one fresh literal");
        let key = context.constant_cache_key(0);
        let address = addresses(&vm)[&key];
        let before = value(&vm, key).as_raw_gc().unwrap();
        assert!(young(&vm, key), "last allocation is a nursery literal");
        for _ in 0..3 {
            prepare(&vm);
        }
        observed_young.push(literal_root_count(&vm, RootSet::Young));
        expected_young.push(1);
        vm.collect_minor_tracing_runtime_roots();
        assert_ne!(
            value(&vm, key).as_raw_gc().unwrap(),
            before,
            "fresh literal actually moves"
        );
        assert_eq!(
            addresses(&vm)[&key],
            address,
            "native cell address is stable"
        );
        assert_contents(&vm, &context, std::slice::from_ref(&constant));
        prepare(&vm);
        observed_young.push(literal_root_count(&vm, RootSet::Young));
        expected_young.push(usize::from(young(&vm, key)));
        vm.collect_minor_tracing_runtime_roots();
        assert!(!young(&vm, key));
        prepare(&vm);
        observed_young.push(literal_root_count(&vm, RootSet::Young));
        expected_young.push(0);
        fresh_contexts.push((context, constant));
    }

    let before_full = addresses(&vm);
    vm.force_gc()
        .expect("full collection retains old literal cells");
    assert_eq!(addresses(&vm), before_full);
    assert_eq!(literal_root_count(&vm, RootSet::All), 68);
    assert_contents(&vm, &anchor, &anchor_constants);
    assert_contents(&vm, &growth, &growth_constants);
    for (context, constant) in &fresh_contexts {
        assert_contents(&vm, context, std::slice::from_ref(constant));
    }
    assert_eq!(
        observed_young, expected_young,
        "minor root work must depend only on unpromoted literals"
    );
}

#[test]
fn literal_source_eviction_removes_mixed_generation_cells_before_reuse() {
    let mut vm = Interpreter::new().expect("literal eviction runtime");
    vm.set_code_eviction_high_water_bytes(0);
    let pinned_constants = vec![string("pinned old literal"), bigint("70000000000000000001")];
    let pinned = vm
        .link_module(
            module("pinned-literals.js", pinned_constants.clone(), 1),
            crate::source_registry::SourceRegistry::default(),
        )
        .unwrap();
    vm.run(&pinned).unwrap();
    let victim_constants = vec![
        bigint("80000000000000000002"),
        string("evicted young literal"),
    ];
    let victim = vm
        .link_evictable_module(
            module("evicted-literals.js", victim_constants, 1),
            crate::source_registry::SourceRegistry::default(),
        )
        .unwrap();
    vm.run(&victim).unwrap();
    vm.collect_minor_tracing_runtime_roots();
    vm.collect_minor_tracing_runtime_roots();
    // A host entry explicitly owns its realm context after returning. Move
    // that owner to the pinned source before dropping the dynamic lease.
    vm.run(&pinned)
        .expect("restore the pinned host-script context");
    let victim_keys = [victim.constant_cache_key(0), victim.constant_cache_key(1)];
    assert!(!young(&vm, victim_keys[0]));
    {
        let _roots = vm.scope_runtime_roots_guard();
        vm.load_bigint_constant_value(&pinned, 1).unwrap();
        vm.load_string_constant_value(&victim, 1).unwrap();
    }
    assert!(young(&vm, victim_keys[1]));
    let before = addresses(&vm);
    assert_eq!(before.len(), 4);
    drop(victim);
    vm.reclaim_dynamic_code()
        .expect("release the dead literal source");
    assert_eq!(vm.code_eviction_stats().evicted_chunks, 1);
    assert!(
        victim_keys
            .iter()
            .all(|key| !vm.literal_cells().contains_key(key))
    );
    let frontier = vm.literal_cells().queued_keys_for_test(&vm.mutator);
    assert!(victim_keys.iter().all(|key| !frontier.contains(key)));
    assert!(
        frontier
            .iter()
            .all(|key| vm.literal_cells().contains_key(key))
    );
    let surviving = addresses(&vm);
    assert_eq!(surviving.len(), 2);
    for (key, address) in &surviving {
        assert_eq!(before[key], *address);
    }
    assert_contents(&vm, &pinned, &pinned_constants);

    let replacement_constants = vec![string("replacement moving literal")];
    let replacement = vm
        .link_evictable_module(
            module("replacement-literals.js", replacement_constants.clone(), 1),
            crate::source_registry::SourceRegistry::default(),
        )
        .unwrap();
    vm.run(&replacement)
        .expect("materialize a new source after eviction");
    assert_eq!(vm.literal_cells().len(), 3);
    let replacement_key = replacement.constant_cache_key(0);
    assert!(young(&vm, replacement_key));
    vm.collect_minor_tracing_runtime_roots();
    vm.force_gc().expect("collect after source identity churn");
    prepare(&vm);
    assert_eq!(literal_root_count(&vm, RootSet::All), 3);
    assert_contents(&vm, &pinned, &pinned_constants);
    assert_contents(&vm, &replacement, &replacement_constants);
    let actual_young = vm
        .literal_cells()
        .keys()
        .filter(|key| young(&vm, **key))
        .count();
    assert_eq!(
        literal_root_count(&vm, RootSet::Young),
        actual_young,
        "evicted frontier keys cannot resurrect or keep old sources in minor roots"
    );
}

#[test]
fn young_large_literal_promotes_in_place_then_leaves_minor_roots() {
    let mut vm = Interpreter::new().expect("large literal runtime");
    let text = "L".repeat(otter_gc::page::LARGE_OBJECT_THRESHOLD + 1);
    let constants = vec![string(&text)];
    let context = vm
        .link_module(
            module("large-literal.js", constants.clone(), 1),
            crate::source_registry::SourceRegistry::default(),
        )
        .unwrap();
    vm.run(&context).expect("materialize a large literal");
    let key = context.constant_cache_key(0);
    assert!(young(&vm, key), "fixture must enter young-large space");
    let address = addresses(&vm)[&key];
    let raw = value(&vm, key).as_raw_gc().unwrap();
    for _ in 0..3 {
        prepare(&vm);
    }
    assert_eq!(literal_root_count(&vm, RootSet::Young), 1);
    assert_eq!(literal_root_count(&vm, RootSet::All), 1);
    vm.collect_minor_tracing_runtime_roots();
    assert!(!young(&vm, key), "young-large cell promotes on this minor");
    assert_eq!(
        value(&vm, key).as_raw_gc().unwrap(),
        raw,
        "large promotion is in place"
    );
    prepare(&vm);
    let young_after = literal_root_count(&vm, RootSet::Young);
    vm.force_gc()
        .expect("full collection of an old large literal");
    assert_eq!(value(&vm, key).as_raw_gc().unwrap(), raw);
    assert_eq!(addresses(&vm)[&key], address);
    assert_contents(&vm, &context, &constants);
    assert_eq!(
        young_after, 0,
        "in-place promotion removes the literal from nursery roots"
    );
}
