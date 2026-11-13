//! Current String proofs before native cached Number conversion.
//!
//! # Contents
//! - Both operand orders with a statically numeric side and a dynamic String.
//! - Cache hits/misses and actual collecting concatenation at strides 1–16.
//! - A numeric guard miss resumes the original Add once without string coercion.
//!
//! # Invariants
//! - Fits and collecting probes execute the same normally admitted generation.
//! - Guard and conversion belong to the same source Add; misses precede effects.
//! - All retained strings use collector-updated handles, including lone surrogates.
//!
//! # See also
//! - `super::native_trace` proves current native execution without interpreter replay.

use super::*;

fn guarded_source() -> BytecodeModule {
    let mut module = source();
    for number_first in [true, false] {
        let mut code = FunctionCodeBuilder::new();
        let number_arg = if number_first { 0 } else { 1 };
        let number = 8;
        code.push(
            Op::BitwiseAndImm,
            &[
                Operand::Register(number),
                Operand::Register(number_arg),
                Operand::Imm32(-1),
            ],
        );
        // Real source work earns the production policy's own callable generation.
        for _ in 0..12 {
            code.push(
                Op::AddImm,
                &[
                    Operand::Register(6),
                    Operand::Register(number),
                    Operand::Imm32(1),
                ],
            );
            code.push(
                Op::SubImm,
                &[
                    Operand::Register(7),
                    Operand::Register(6),
                    Operand::Imm32(1),
                ],
            );
        }
        code.push(
            Op::Add,
            &[
                Operand::Register(5),
                Operand::Register(if number_first { number } else { 0 }),
                Operand::Register(if number_first { 1 } else { number }),
            ],
        );
        code.push(Op::ReturnValue, &[Operand::Register(5)]);
        module.functions.push(Function {
            id: module.functions.len() as u32,
            name: format!("guardedConcat{number_first}"),
            param_count: 5,
            scratch: 9,
            is_strict: true,
            code: code.finish(),
            ..Default::default()
        });
    }
    module
}

#[test]
fn string_guards_preserve_numeric_add_and_native_moving_roots() {
    let mut vm = Interpreter::new().expect("guarded concat VM");
    vm.gc_heap_mut().set_gc_stress(0, true);
    let source = vm
        .link_module(
            guarded_source(),
            otter_vm::source_registry::SourceRegistry::default(),
        )
        .expect("verified guarded concat source");
    // SAFETY: this fixture owns the heap and stationary handle arena until all
    // locals drop; no raw GC value is retained across the measured allocation.
    let scope = unsafe { HandleScope::from_ptr(vm.gc_heap_mut().handle_stack_ptr()) };
    let text = scope.local(string(&mut vm, &[0x70, 0x3a]));
    let ids = [source.main().id + 17, source.main().id + 18];
    for (index, fid) in ids.into_iter().enumerate() {
        let (a, b) = if index == 0 {
            (Value::number_i32(7), value(&text))
        } else {
            (value(&text), Value::number_i32(7))
        };
        let _ = invoke(&mut vm, &source, fid, a, b);
    }
    Arc::new(OtterJitCompiler::production_tiered()).install(&mut vm);
    vm.set_jit_debug_request(JitDebugRequest::artifacts().with_events(true));
    vm.begin_jit_debug_capture();
    for warm in 0..16000 {
        for (index, fid) in ids.into_iter().enumerate() {
            let (a, b) = if index == 0 {
                (Value::number_i32(7), value(&text))
            } else {
                (value(&text), Value::number_i32(7))
            };
            vm.run_callable_sync(
                &source,
                &Value::function(fid),
                Value::undefined(),
                smallvec![a, b, Value::number_i32(0), a, b],
            )
            .unwrap_or_else(|error| {
                panic!(
                    "guarded concat warm={warm} fid={fid}: {error:?}; {:?}",
                    vm.take_jit_debug_report()
                )
            });
        }
    }
    let generations = ids.map(|fid| current(&vm, fid, true));
    let artifacts = vm.take_jit_artifacts().expect("guarded concat artifacts");
    for generation in &generations {
        let bundle = artifacts
            .bundles()
            .iter()
            .find(|bundle| bundle.manifest().code_object_id() == generation.code_object_id)
            .expect("exact current generation artifact");
        assert_artifact(bundle, true, true);
        let ir = std::str::from_utf8(
            bundle
                .file(JitArtifactFileName::OptimizedIr)
                .expect("optimizing IR")
                .contents(),
        )
        .unwrap();
        assert!(
            ir.find(" = CheckString ").unwrap() < ir.find(" = NumberToString ").unwrap(),
            "{ir}"
        );
        let map = json(bundle, JitArtifactFileName::CodeMap);
        let rows = map["regions"].as_array().unwrap();
        let guard = rows
            .iter()
            .find(|row| {
                row["operation"]
                    .as_str()
                    .is_some_and(|operation| operation.contains("CheckString"))
            })
            .expect("emitted guard");
        let conversion = rows
            .iter()
            .find(|row| {
                row["operation"]
                    .as_str()
                    .is_some_and(|operation| operation.contains("NumberToString"))
            })
            .expect("emitted conversion");
        assert_eq!(guard["bytePc"], conversion["bytePc"]);
        assert_eq!(guard["functionId"], generation.function_id);
        assert!(guard["startOffset"].as_u64().unwrap() < guard["endOffset"].as_u64().unwrap());
        assert!(
            guard["endOffset"].as_u64().unwrap() <= conversion["startOffset"].as_u64().unwrap()
        );
    }
    let _ = vm.take_jit_debug_report();
    for (index, generation) in generations.iter().enumerate() {
        for stride in 1..=16 {
            for number in [7, 100000 + stride as i32 + index as i32 * 100] {
                vm.gc_heap_mut().set_gc_stress(0, true);
                let units = [vec![0x100; 25], vec![0xd800, 0xdc00, 0xd800]].concat();
                let text = scope.local(string(&mut vm, &units));
                let old_offset = text.get().offset();
                let decimal: Vec<_> = number.to_string().encode_utf16().collect();
                let expected = if index == 0 {
                    [decimal, units].concat()
                } else {
                    [units, decimal].concat()
                };
                vm.gc_heap_mut().set_gc_stress(stride, true);
                for _ in 1..stride {
                    vm.gc_heap_mut()
                        .alloc(otter_gc::test_support::OpaqueLeaf { payload: 0 })
                        .unwrap();
                }
                let before = vm.gc_stats_snapshot();
                let calls = vm.jit_runtime_stats();
                let (a, b) = if index == 0 {
                    (Value::number_i32(number), value(&text))
                } else {
                    (value(&text), Value::number_i32(number))
                };
                let result = native_trace(&mut vm, &source, generation, a, b);
                assert_eq!(utf16(&vm, result), expected);
                let result = scope.local(result.as_string_gc().unwrap());
                let after = vm.gc_stats_snapshot();
                assert!(after.minor_gc_cycles > before.minor_gc_cycles);
                assert!(after.minor_slot_updates > before.minor_slot_updates);
                assert_ne!(text.get().offset(), old_offset);
                assert_eq!(
                    vm.jit_runtime_stats().alloc_value_stub_ok - calls.alloc_value_stub_ok,
                    if number == 7 { 1 } else { 2 },
                    "cache hit/miss plus concat"
                );
                vm.gc_heap_mut().set_gc_stress(0, true);
                vm.force_gc().expect("retain native result across full GC");
                assert_eq!(utf16(&vm, value(&result)), expected);
            }
        }
        let trace = Arc::new(Mutex::new(Trace::default()));
        vm.set_tracer(Some(Box::new(Tracer(trace.clone()))));
        vm.begin_jit_debug_capture();
        let calls = vm.jit_runtime_stats();
        let result = invoke(
            &mut vm,
            &source,
            generation.function_id,
            Value::number_i32(7),
            Value::number_i32(35),
        );
        vm.set_tracer(None);
        assert_eq!(result.as_i32(), Some(42));
        assert_eq!(
            vm.jit_runtime_stats().alloc_value_stub_ok,
            calls.alloc_value_stub_ok
        );
        let report = vm.take_jit_debug_report().unwrap();
        assert!(!report.truncated());
        assert_eq!(report.dropped_events(), 0);
        let exits: Vec<_> = report
            .events()
            .iter()
            .filter(|event| {
                matches!(event,
            JitDebugEvent::EnteredGenerationDeopt { callee_function_id, .. }
                if *callee_function_id == generation.function_id)
            })
            .collect();
        assert_eq!(exits.len(), 1, "{:?}", report.events());
        assert!(
            matches!(exits[0],
            JitDebugEvent::EnteredGenerationDeopt {
                callee_code_object_id,
                callee_tier: JitDebugTier::Optimizing,
                callee_resume_pc: 25,
                exit_reason: otter_vm::native_abi::ExitReason::TypeMismatch,
                exit_action: otter_vm::native_abi::ExitAction::Recompile,
                ..
            } if *callee_code_object_id == generation.code_object_id),
            "numeric pair must fail the current String proof: {:?}",
            report.events()
        );
        assert!(
            !report.events().iter().any(|event| matches!(
                event,
                JitDebugEvent::Bail { .. } | JitDebugEvent::InlineDeoptFrame { .. }
            )),
            "the exact direct generation owns this exit: {:?}",
            report.events()
        );
        let trace = trace.lock().unwrap();
        assert_eq!(
            trace
                .steps
                .iter()
                .filter(|(fid, _, op)| *fid == generation.function_id && *op == Op::Add)
                .count(),
            1,
            "only the original Add resumes: {:?}",
            trace.steps
        );
    }
}
