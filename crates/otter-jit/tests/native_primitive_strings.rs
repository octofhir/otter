//! Real current Graph/Template primitive string operations and moving roots.
//!
//! # Contents
//! - Production-policy native concat fits, collecting probes and mixed Numbers.
//! - Exact UTF-16 flat/rope/slice ordering and unordered numeric comparisons.
//! - Own generation artifacts, managed accounting and complete source traces.
//!
//! # Invariants
//! All retained inputs use collector-rewritten handles. The measured concat
//! function contains exactly one managed allocation operation, so actual minor
//! movement and the one typed Success are attributed to that source Add. Alias
//! comparisons after Add consume canonical native homes before returning.
//! Ordering cannot allocate, fill caches or collect even when stress is armed.
//! Both tiers use their production selection policy, never fixture thresholds.
//!
//! # See also
//! - `allocation::string_group_tests` verifies complete native payload bytes.
//! - `runtime_stubs::string` owns primitive conversion and NoAlloc ordering.

#![cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]

use otter_bytecode::{BytecodeModule, Function, FunctionCodeBuilder, Op, Operand, SourceKind};
use otter_gc::{HandleScope, Local};
use otter_jit::OtterJitCompiler;
use otter_vm::{
    ExecutionContext, Interpreter, JitArtifactBundle, JitArtifactFileName,
    JitCodeGenerationSnapshot, JitDebugEvent, JitDebugRequest, JitDebugTier, JsString,
    NativeCallInfo, NativeCtx, NativeError, Value,
    inspect::{StepEvent, StepTracer},
    jit::JitStringLayout,
    native_abi::{
        CodeLifetimeState, NativeFrameKind, STUB_PRIMITIVE_STRING_ORDER, STUB_STRING_CONCAT_ALLOC,
    },
    string::{JsStringBody, JsStringBodyRepr, JsStringHandle},
};
use serde_json::Value as Json;
use smallvec::smallvec;
use std::sync::{Arc, Mutex};

const OPERATIONS: [Op; 6] = [
    Op::LessThan,
    Op::LessEq,
    Op::GreaterThan,
    Op::GreaterEq,
    Op::LooseEqual,
    Op::LooseNotEqual,
];
fn source() -> BytecodeModule {
    let mut main = FunctionCodeBuilder::new();
    main.push(Op::ReturnUndefined, &[]);
    let mut functions = vec![Function {
        id: 0,
        name: "<main>".into(),
        code: main.finish(),
        ..Default::default()
    }];
    for (index, op) in std::iter::once(Op::Add)
        .chain(std::iter::once(Op::Add))
        .chain(OPERATIONS)
        .enumerate()
    {
        let mut code = FunctionCodeBuilder::new();
        code.push(Op::LoadInt32, &[Operand::Register(6), Operand::Imm32(0)]);
        // Native profitability sees ordinary source work. The first Add has
        // only numeric operands; it is distinct from the tested operation.
        for pair in 0..10u16 {
            code.push(
                Op::Add,
                &[
                    Operand::Register(7 + 2 * pair),
                    Operand::Register(6),
                    Operand::Register(2),
                ],
            );
            code.push(
                Op::Sub,
                &[
                    Operand::Register(8 + 2 * pair),
                    Operand::Register(7 + 2 * pair),
                    Operand::Register(2),
                ],
            );
        }
        code.push(
            op,
            &[
                Operand::Register(5),
                Operand::Register(0),
                Operand::Register(1),
            ],
        );
        if index == 0 {
            code.push(
                Op::Equal,
                &[
                    Operand::Register(29),
                    Operand::Register(0),
                    Operand::Register(3),
                ],
            );
            let bad_left = code.push(Op::JumpIfFalse, &[Operand::Imm32(0), Operand::Register(29)]);
            code.push(
                Op::Equal,
                &[
                    Operand::Register(30),
                    Operand::Register(1),
                    Operand::Register(4),
                ],
            );
            let bad_right = code.push(Op::JumpIfFalse, &[Operand::Imm32(0), Operand::Register(30)]);
            code.push(Op::ReturnValue, &[Operand::Register(5)]);
            let bad = code.push(Op::ReturnUndefined, &[]);
            for at in [bad_left, bad_right] {
                assert!(code.set_operand(at, 0, Operand::Imm32((bad - at - 1) as i32)));
            }
        } else {
            code.push(Op::ReturnValue, &[Operand::Register(5)]);
        }
        functions.push(Function {
            id: index as u32 + 1,
            name: format!("primitive{index}"),
            param_count: 5,
            scratch: 32,
            is_strict: true,
            code: code.finish(),
            ..Default::default()
        });
    }
    // A normal known call enters each baseline worker's own generation. The
    // worker has >20 source operations, so this bridge cannot body-inline it.
    // Its arguments/callee are ordinary source registers, never a JIT hook.
    for index in 0..8u32 {
        let mut code = FunctionCodeBuilder::new();
        code.push(
            Op::Call,
            &[
                Operand::Register(6),
                Operand::Register(5),
                Operand::ConstIndex(5),
                Operand::Register(0),
                Operand::Register(1),
                Operand::Register(2),
                Operand::Register(3),
                Operand::Register(4),
            ],
        );
        code.push(Op::ReturnValue, &[Operand::Register(6)]);
        functions.push(Function {
            id: 9 + index,
            name: format!("bridge{index}"),
            param_count: 6,
            scratch: 7,
            is_strict: true,
            code: code.finish(),
            ..Default::default()
        });
    }
    BytecodeModule {
        module: "file:///native-primitive-strings.js".into(),
        template_sites: Vec::new(),
        source_kind: SourceKind::JavaScript,
        functions,
        function_source: None,
        constants: Vec::new(),
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
    }
}
fn invoke(vm: &mut Interpreter, source: &ExecutionContext, fid: u32, a: Value, b: Value) -> Value {
    vm.run_callable_sync(
        source,
        &Value::function(fid),
        Value::undefined(),
        smallvec![a, b, Value::number_i32(0), a, b],
    )
    .expect("primitive source completion")
}
fn bridge_id(source: &ExecutionContext, fid: u32) -> u32 {
    source.main().id + 9 + (fid - source.main().id - 1)
}
fn invoke_bridge(
    vm: &mut Interpreter,
    source: &ExecutionContext,
    fid: u32,
    a: Value,
    b: Value,
) -> Value {
    vm.run_callable_sync(
        source,
        &Value::function(bridge_id(source, fid)),
        Value::undefined(),
        smallvec![a, b, Value::number_i32(0), a, b, Value::function(fid)],
    )
    .expect("known baseline worker completion")
}
fn current(vm: &Interpreter, fid: u32, optimizing: bool) -> JitCodeGenerationSnapshot {
    let rows: Vec<_> = vm
        .jit_code_generation_snapshot()
        .into_iter()
        .filter(|g| {
            g.function_id == fid
                && g.tier
                    == if optimizing {
                        NativeFrameKind::Optimizing
                    } else {
                        NativeFrameKind::Baseline
                    }
                && g.lifecycle == CodeLifetimeState::Installed
                && g.linked
        })
        .collect();
    assert_eq!(
        rows.len(),
        1,
        "one current own requested generation fid={fid}: {rows:?}"
    );
    rows.into_iter().next().unwrap()
}
fn string(vm: &mut Interpreter, units: &[u16]) -> JsStringHandle {
    NativeCtx::with_host_context(vm, NativeCallInfo::default_call(), None, |ctx| {
        // Engine fixture for WTF-16, including isolated surrogate units. The
        // production native context supplies all runtime roots; the returned
        // typed body is parked in the handle arena before another allocation.
        let string = JsString::from_utf16_units(units, ctx.interp_mut().gc_heap_mut())
            .expect("fixture WTF-16 body");
        Ok::<Value, NativeError>(Value::string(string))
    })
    .unwrap()
    .as_string_gc()
    .unwrap()
}
fn value(local: &Local<'_, JsStringBody>) -> Value {
    Value::string_gc(local.get())
}
fn utf16(vm: &Interpreter, value: Value) -> Vec<u16> {
    value
        .as_string(vm.gc_heap())
        .expect("string result")
        .to_utf16_vec(vm.gc_heap())
}
fn json(bundle: &JitArtifactBundle, name: JitArtifactFileName) -> Json {
    serde_json::from_slice(bundle.file(name).unwrap().contents()).unwrap()
}
fn assert_artifact(bundle: &JitArtifactBundle, optimizing: bool, concat: bool) {
    assert_eq!(
        bundle.manifest().tier(),
        if optimizing {
            JitDebugTier::Optimizing
        } else {
            JitDebugTier::Template
        }
    );
    if optimizing {
        let ir = std::str::from_utf8(
            bundle
                .file(JitArtifactFileName::OptimizedIr)
                .unwrap()
                .contents(),
        )
        .unwrap();
        assert_eq!(
            ir.lines()
                .filter(|line| line.contains(if concat {
                    " = PrimitiveAdd "
                } else {
                    " = PrimitiveCompare("
                }))
                .count(),
            1,
            "{ir}"
        );
    }
    let descriptor = if concat {
        STUB_STRING_CONCAT_ALLOC
    } else {
        STUB_PRIMITIVE_STRING_ORDER
    };
    let relocations = json(bundle, JitArtifactFileName::Relocations);
    assert!(
        relocations["relocations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["target"]["id"] == descriptor.id
                && r["target"]["signature"] == if concat { "allocValue3" } else { "leafValue2" }),
        "current own primitive operation has its exact typed edge"
    );
    let map = json(bundle, JitArtifactFileName::CodeMap);
    assert!(
        map["regions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["kind"] == "instruction"
                && r["functionId"] == bundle.manifest().function_id()
                && r["startOffset"].as_u64().unwrap() < r["endOffset"].as_u64().unwrap()),
        "current source really emits executable instruction regions"
    );
}
#[derive(Default)]
struct Trace {
    steps: Vec<(u32, u32, Op)>,
}
struct Tracer(Arc<Mutex<Trace>>);
impl StepTracer for Tracer {
    fn on_step(&mut self, event: &StepEvent<'_>) {
        self.0
            .lock()
            .unwrap()
            .steps
            .push((event.function_id, event.byte_pc, event.op));
    }
}
fn native_trace(
    vm: &mut Interpreter,
    source: &ExecutionContext,
    generation: &JitCodeGenerationSnapshot,
    a: Value,
    b: Value,
) -> Value {
    let before = vm.jit_runtime_stats();
    let trace = Arc::new(Mutex::new(Trace::default()));
    vm.set_tracer(Some(Box::new(Tracer(trace.clone()))));
    vm.begin_jit_debug_capture();
    let optimizing = generation.tier == NativeFrameKind::Optimizing;
    let own_before = current(vm, generation.function_id, optimizing);
    assert!(own_before.current_entry && own_before.call_entry_offset.is_some());
    let bridge_before = (!optimizing).then(|| {
        let bridge = current(vm, bridge_id(source, generation.function_id), false);
        assert!(bridge.current_entry && bridge.call_entry_offset.is_some());
        bridge
    });
    // SourceWork charges Template prefixes only. Graph execution is proved
    // by exact current mapping + calibrated complete trace + observable result;
    // Template additionally retains its exact per-generation bridge counts.
    let work = (!optimizing).then(|| {
        source
            .jit_compile_snapshot(generation.function_id)
            .unwrap()
            .code_block
            .source_work()
            .clone()
    });
    let work_before = work.as_ref().map(|work| work.total());
    let returned = if optimizing {
        invoke(vm, source, generation.function_id, a, b)
    } else {
        invoke_bridge(vm, source, generation.function_id, a, b)
    };
    vm.set_tracer(None);
    let after = vm.jit_runtime_stats();
    let report = vm.take_jit_debug_report().unwrap();
    assert!(!report.truncated());
    assert_eq!(report.dropped_events(), 0);
    assert!(
        !report.events().iter().any(|e| matches!(
            e,
            JitDebugEvent::Bail { .. }
                | JitDebugEvent::EnteredGenerationDeopt { .. }
                | JitDebugEvent::InlineDeoptFrame { .. }
                | JitDebugEvent::CompilePrepared { .. }
        )),
        "measured source stays in the same installed generation: {:?}",
        report.events()
    );
    assert!(
        trace.lock().unwrap().steps.is_empty(),
        "complete dispatch observer sees no interpreter replay"
    );
    if let (Some(work), Some(work_before)) = (&work, work_before) {
        assert!(
            work.total() > work_before,
            "exact own Template source-prefix charge"
        );
    }
    if let Some(bridge_before) = bridge_before {
        // Both callable private entries execute: the canonical synchronous
        // trampoline enters the bridge, then its emitted Call enters the
        // worker. The global counter is the exact sum of those two cells.
        assert_eq!(
            after.generated_template_entries,
            before.generated_template_entries + 2,
            "one compiled bridge entry and one compiled worker entry"
        );
        assert_eq!(
            after.generated_template_returns,
            before.generated_template_returns + 2
        );
        assert_eq!(
            after.generated_template_deopts,
            before.generated_template_deopts
        );
        let bridge_after = current(vm, bridge_before.function_id, false);
        assert!(bridge_after.current_entry);
        assert_eq!(bridge_after.code_object_id, bridge_before.code_object_id);
        assert_eq!(
            bridge_after.call_entry_offset,
            bridge_before.call_entry_offset
        );
        assert_eq!(
            bridge_after.generated_entries,
            bridge_before.generated_entries + 1
        );
        assert_eq!(
            bridge_after.generated_returns,
            bridge_before.generated_returns + 1
        );
        assert_eq!(
            bridge_after.generated_deopts,
            bridge_before.generated_deopts
        );
        let own_after = current(vm, generation.function_id, false);
        assert!(own_after.current_entry);
        assert_eq!(own_after.call_entry_offset, own_before.call_entry_offset);
        assert_eq!(
            own_after.generated_entries,
            own_before.generated_entries + 1,
            "the exact worker generation executes, without bridge body splicing"
        );
        assert_eq!(
            own_after.generated_returns,
            own_before.generated_returns + 1
        );
        assert_eq!(own_after.generated_deopts, own_before.generated_deopts);
    }
    assert_eq!(
        current(vm, generation.function_id, optimizing).code_object_id,
        generation.code_object_id
    );
    returned
}
fn predicate(index: usize, order: i32) -> bool {
    match index {
        0 => order < 0,
        1 => order <= 0,
        2 => order == 1,
        3 => (0..=1).contains(&order),
        4 => order == 0,
        5 => order != 0,
        _ => unreachable!(),
    }
}
fn run(optimizing: bool) {
    let mut vm = Interpreter::new().expect("string proof VM");
    vm.gc_heap_mut().set_gc_stress(0, true);
    let source = vm
        .link_module(
            source(),
            otter_vm::source_registry::SourceRegistry::default(),
        )
        .expect("normal verified primitive source");
    // SAFETY: this sole heap and its handle stack outlive all typed locals;
    // no Local/GC handle crosses a contributor-facing public boundary.
    let scope = unsafe { HandleScope::from_ptr(vm.gc_heap_mut().handle_stack_ptr()) };
    let left = scope.local(string(&mut vm, &[0x61, 0x62, 0x63]));
    let right = scope.local(string(&mut vm, &[0x64, 0x65]));
    let oracle = Arc::new(Mutex::new(Trace::default()));
    vm.set_tracer(Some(Box::new(Tracer(oracle.clone()))));
    let oracle_result = invoke(
        &mut vm,
        &source,
        source.main().id + 1,
        value(&left),
        value(&right),
    );
    vm.set_tracer(None);
    assert_eq!(
        utf16(&vm, oracle_result),
        "abcde".encode_utf16().collect::<Vec<_>>()
    );
    assert!(
        oracle
            .lock()
            .unwrap()
            .steps
            .iter()
            .any(|(fid, _, op)| *fid == source.main().id + 1 && *op == Op::Add),
        "the actual prewarm source Add is observed by the same complete dispatch hook"
    );
    // Calibrate every requested worker before any compiler can install it;
    // this is the same full observer used by each isolated native probe.
    for index in 1..if optimizing { 8 } else { 6 } {
        let fid = source.main().id + 1 + index;
        let trace = Arc::new(Mutex::new(Trace::default()));
        vm.set_tracer(Some(Box::new(Tracer(trace.clone()))));
        let _ = invoke(&mut vm, &source, fid, value(&left), value(&right));
        vm.set_tracer(None);
        assert!(
            trace
                .lock()
                .unwrap()
                .steps
                .iter()
                .any(|(observed, _, _)| *observed == fid)
        );
    }
    Arc::new(if optimizing {
        OtterJitCompiler::production_tiered()
    } else {
        OtterJitCompiler::template_only()
    })
    .install(&mut vm);
    vm.set_jit_debug_request(JitDebugRequest::artifacts().with_events(true));
    vm.begin_jit_debug_capture();
    let count = if optimizing { 8 } else { 6 };
    for warm in 0..16000 {
        for index in 0..count {
            let fid = source.main().id + 1 + index;
            let (a, b) = if index == 1 && warm % 2 == 0 {
                (Value::number_i32(7), value(&right))
            } else {
                (value(&left), value(&right))
            };
            let _ = invoke(&mut vm, &source, fid, a, b);
        }
    }
    if !optimizing {
        for _ in 0..16000 {
            for index in 0..count {
                let _ = invoke_bridge(
                    &mut vm,
                    &source,
                    source.main().id + 1 + index,
                    value(&left),
                    value(&right),
                );
            }
        }
        for index in 0..count {
            let bridge = current(&vm, source.main().id + 9 + index, false);
            assert!(bridge.linked);
        }
    }
    let generations: Vec<_> = (0..count)
        .map(|index| current(&vm, source.main().id + 1 + index, optimizing))
        .collect();
    let artifacts = vm.take_jit_artifacts().unwrap();
    for (index, generation) in generations.iter().enumerate() {
        let bundle = artifacts
            .bundles()
            .iter()
            .find(|b| b.manifest().code_object_id() == generation.code_object_id)
            .unwrap();
        assert_artifact(bundle, optimizing, index < 2);
        assert!(generation.current_entry);
        let offset = generation
            .call_entry_offset
            .expect("actual worker private-call capability");
        assert!(
            (offset as usize)
                < bundle
                    .file(JitArtifactFileName::Code)
                    .unwrap()
                    .contents()
                    .len()
        );
        assert_eq!(
            json(bundle, JitArtifactFileName::CodeMap)["callEntryOffset"].as_u64(),
            Some(u64::from(offset))
        );
    }
    let _ = vm.take_jit_debug_report();
    // Verify the same full observer remains active with the production hook
    // installed. This real one-op main has no warm/admitted native generation.
    let calibration = Arc::new(Mutex::new(Trace::default()));
    vm.set_tracer(Some(Box::new(Tracer(calibration.clone()))));
    let main = vm
        .run_callable_sync(
            &source,
            &Value::function(source.main().id),
            Value::undefined(),
            smallvec![],
        )
        .expect("short ordinary source with installed JIT");
    vm.set_tracer(None);
    assert!(main.is_undefined());
    assert_eq!(
        calibration.lock().unwrap().steps.as_slice(),
        &[(source.main().id, 0, Op::ReturnUndefined)]
    );
    for generation in &generations {
        assert_eq!(
            current(&vm, generation.function_id, optimizing).code_object_id,
            generation.code_object_id
        );
    }
    let layout = JitStringLayout::default();
    for (a, b, latin) in [
        (vec![0x61, 0x62], vec![0x63], true),
        (vec![0x100, 0xd800], vec![0x61, 0xdc00], false),
        (vec![0x61; 25], vec![0x100; 13], false),
    ] {
        let a = scope.local(string(&mut vm, &a));
        let b = scope.local(string(&mut vm, &b));
        let av = value(&a);
        let bv = value(&b);
        let mut expected = utf16(&vm, av);
        expected.extend(utf16(&vm, bv));
        let window = vm.gc_heap_mut().machine_allocation_window();
        // SAFETY: actual stable heap LAB descriptor, read-only premise.
        assert!(
            unsafe { (*window.lab).remaining() } >= layout.cell_bytes as usize,
            "real native fit headroom"
        );
        let before = vm.gc_stats_snapshot();
        let calls = vm.jit_runtime_stats();
        let result = native_trace(&mut vm, &source, &generations[0], av, bv);
        assert_eq!(utf16(&vm, result), expected);
        let after = vm.gc_stats_snapshot();
        assert_eq!(after.minor_gc_cycles, before.minor_gc_cycles);
        assert_eq!(
            after.by_type[layout.string_type_tag as usize].alloc_count_total,
            before.by_type[layout.string_type_tag as usize].alloc_count_total + 1
        );
        assert_eq!(
            vm.jit_runtime_stats().alloc_value_stub_ok,
            calls.alloc_value_stub_ok,
            "native fit does not call allocating Probe"
        );
        let result = scope.local(result.as_string_gc().unwrap());
        vm.gc_heap().read_payload(result.get(),|body| {
            assert_eq!(body.len as usize,expected.len());
            if expected.len()>(if latin {layout.inline_latin1_cap}else{layout.inline_flat_cap}) as usize {
                assert!(matches!(body.repr,JsStringBodyRepr::Cons{left,right,depth:1} if left==a.get() && right==b.get()));
            } else if latin {assert!(matches!(body.repr,JsStringBodyRepr::InlineLatin1(_)));}
            else {assert!(matches!(body.repr,JsStringBodyRepr::InlineFlat(_)));}
        });
    }
    for stride in 1..=16 {
        vm.gc_heap_mut().set_gc_stress(0, true);
        let a = scope.local(string(&mut vm, &[0x61; 25]));
        let b = scope.local(string(&mut vm, &[0x100, 0xd800, 0x64]));
        let before_offsets = [a.get().offset(), b.get().offset()];
        let mut expected = utf16(&vm, value(&a));
        expected.extend(utf16(&vm, value(&b)));
        vm.gc_heap_mut().set_gc_stress(stride, true);
        // Genuine stress-counter priming cannot collect before the tested
        // allocation: the next allocation is exactly the stride boundary.
        for _ in 1..stride {
            vm.gc_heap_mut()
                .alloc(otter_gc::test_support::OpaqueLeaf { payload: 0 })
                .unwrap();
        }
        let before = vm.gc_stats_snapshot();
        let calls = vm.jit_runtime_stats();
        let result = native_trace(&mut vm, &source, &generations[0], value(&a), value(&b));
        assert_eq!(utf16(&vm, result), expected);
        let after = vm.gc_stats_snapshot();
        assert!(after.minor_gc_cycles > before.minor_gc_cycles);
        assert!(after.minor_slot_updates > before.minor_slot_updates);
        assert_ne!(a.get().offset(), before_offsets[0]);
        assert_ne!(b.get().offset(), before_offsets[1]);
        assert_eq!(
            vm.jit_runtime_stats().alloc_value_stub_ok,
            calls.alloc_value_stub_ok + 1,
            "only the measured source Add crosses the collecting typed boundary"
        );
        let result = scope.local(result.as_string_gc().unwrap());
        vm.gc_heap_mut().set_gc_stress(0, true);
        vm.force_gc().unwrap();
        assert_eq!(
            utf16(&vm, value(&result)),
            expected,
            "real post-fit/cold cells traverse and survive full GC"
        );
    }
    vm.gc_heap_mut().set_gc_stress(0, true);
    let empty = scope.local(string(&mut vm, &[]));
    let number_prefix = scope.local(string(&mut vm, &[0x70, 0x3a]));
    for (a, b, expected) in [
        (value(&number_prefix), Value::number_f64(-0.0), "p:0"),
        (Value::number_i32(1023), value(&number_prefix), "1023p:"),
        (value(&number_prefix), Value::number_f64(f64::NAN), "p:NaN"),
    ] {
        let result = native_trace(&mut vm, &source, &generations[1], a, b);
        assert_eq!(
            utf16(&vm, result),
            expected.encode_utf16().collect::<Vec<_>>()
        );
    }
    for (a, b, expected) in [
        (
            Value::number_i32(i32::MAX),
            Value::number_i32(1),
            2147483648.0,
        ),
        (Value::number_f64(-0.0), Value::number_f64(-0.0), -0.0),
        (Value::number_f64(f64::NAN), Value::number_i32(0), f64::NAN),
    ] {
        let result = native_trace(&mut vm, &source, &generations[1], a, b)
            .as_number()
            .unwrap()
            .as_f64();
        if expected.is_nan() {
            assert!(result.is_nan());
        } else {
            assert_eq!(result.to_bits(), expected.to_bits());
        }
    }
    let units = [
        vec![0x61; 25],
        vec![0x100, 0xd800],
        vec![0x100, 0xe000],
        vec![0x100, 0xd800],
        "-0".encode_utf16().collect(),
        "NaN".encode_utf16().collect(),
        "0x10".encode_utf16().collect(),
    ];
    let texts: Vec<_> = units
        .iter()
        .map(|s| scope.local(string(&mut vm, s)))
        .collect();
    let a = value(&texts[0]).as_string(vm.gc_heap()).unwrap();
    let b = value(&right).as_string(vm.gc_heap()).unwrap();
    let rope = scope.local(JsString::concat(a, b, vm.gc_heap_mut()).unwrap().handle());
    let rope_string = value(&rope).as_string(vm.gc_heap()).unwrap();
    let sliced = scope.local(rope_string.slice(1, 23, vm.gc_heap_mut()).unwrap().handle());
    let slice_text = scope.local(string(&mut vm, &[0x61; 23]));
    let cases = [
        (value(&texts[1]), value(&texts[2]), -1),
        (value(&texts[1]), value(&texts[3]), 0),
        (value(&rope), value(&texts[0]), 1),
        (value(&sliced), value(&slice_text), 0),
        (value(&empty), Value::number_i32(0), 0),
        (value(&texts[4]), Value::number_f64(-0.0), 0),
        (value(&texts[5]), Value::number_i32(0), 2),
        (Value::number_f64(f64::NAN), value(&empty), 2),
        (value(&texts[6]), Value::number_i32(16), 0),
        (Value::number_f64(-0.0), Value::number_i32(0), 0),
    ];
    for stress in [0, 1] {
        vm.gc_heap_mut().set_gc_stress(stress, true);
        for (a, b, order) in cases {
            for index in 0..count as usize - 2 {
                let before = vm.gc_stats_snapshot();
                let result = native_trace(&mut vm, &source, &generations[2 + index], a, b);
                assert_eq!(
                    result.as_boolean(),
                    Some(predicate(index, order)),
                    "actual condition {index} order{order}"
                );
                let after = vm.gc_stats_snapshot();
                assert_eq!(after.alloc_bytes_total, before.alloc_bytes_total);
                assert_eq!(
                    after.minor_gc_cycles, before.minor_gc_cycles,
                    "ordering remains NoAlloc under stress"
                );
            }
        }
    }
}
#[test]
fn current_graph_primitive_strings_fit_collect_and_order_exact_utf16_and_numbers() {
    run(true);
}
#[test]
fn current_template_primitive_strings_fit_collect_and_order_exact_utf16_and_numbers() {
    run(false);
}
