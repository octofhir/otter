//! Real current native group refusal and canonical first-member completion.
//!
//! # Contents
//! - A verifier-admitted function with two consecutive fixed source allocations.
//! - Actual own Graph installation/entry, first-source recovery and exact homes.
//! - A real small-cage promotion failure followed by one legal source prefix.
//!
//! # Invariants
//! This isolated binary owns its cage. Pressure consists of real managed cells;
//! no collector mock, failure injection or compiled replay is used. The native
//! group initializes nothing before admission. Its failure resumes the first
//! original instruction; the second allocation's actual OOM is caught in source
//! and returns the completed first object. Successful execution reads both
//! distinct members before returning the first, keeping its tagged projection
//! live through the current optimizer. Observers retain owned scalar data.
//!
//! # See also
//! - `graph::allocation_groups` forms groups before register allocation.
//! - `GcHeap::ensure_machine_allocation_with_roots` owns their sole refill.
//! - GC's separate failure fixture verifies exact failed-collector accounting.

#![cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]

use otter_bytecode::{
    BytecodeModule, ExceptionHandler, Function, FunctionCodeBuilder, Op, Operand, SourceKind,
    encoding::measure_wordcode_function,
};
use otter_gc::{
    GcPauseKind, GcPauseOutcome, GcPauseTrigger, HandleScope, OutOfMemory, RootScope,
    SafeTraceable, init_cage_with_size,
    page::{CELL_SIZE, LARGE_OBJECT_THRESHOLD, PageHeader, page_base_from_offset},
};
use otter_jit::OtterJitCompiler;
use otter_vm::{
    ExecutionContext, Interpreter, JitArtifactBundle, JitArtifactFileName,
    JitCodeGenerationSnapshot, JitDebugEvent, JitDebugRequest, JitDebugTier, NativeCallInfo,
    NativeCtx, NativeError, Value,
    inspect::{StepEvent, StepTracer},
    jit::JitEmptyObjectAllocationPlan,
    native_abi::{CodeLifetimeState, NativeFrameKind, STUB_ALLOC_GROUP_ENSURE},
};
use serde_json::Value as Json;
use smallvec::smallvec;
use std::sync::{Arc, Mutex};

struct PressureCell {
    word: u64,
}
impl SafeTraceable for PressureCell {
    const TYPE_TAG: u8 = 0xeb;
    fn trace_slots_safe(&mut self, _: &mut otter_gc::raw::SlotVisitor<'_>) {}
}

fn source() -> (BytecodeModule, Box<[u32]>) {
    let mut main = FunctionCodeBuilder::new();
    main.push(Op::ReturnUndefined, &[]);
    let mut code = FunctionCodeBuilder::new();
    code.push(Op::LoadUndefined, &[Operand::Register(3)]);
    let first = code.push(Op::NewObject, &[Operand::Register(3)]);
    let second = code.push(Op::NewObject, &[Operand::Register(4)]);
    assert_eq!(
        second,
        first + 1,
        "actual consecutive allocation source instructions"
    );
    assert_eq!((first, second), (1, 2), "unchanged original recovery PCs");
    // Both successfully completed objects are real inputs to observable source
    // control. The failure return distinguishes an aliased group from two
    // distinct cells; the ordinary success path still returns the first.
    // Current Graph passes retain StrictEqual's two tagged SSA inputs and its
    // branch consumer. No constant or oddball operand allows builder folding.
    let comparison = code.push(
        Op::Equal,
        &[
            Operand::Register(30),
            Operand::Register(3),
            Operand::Register(4),
        ],
    );
    assert_eq!(comparison, 3);
    code.push(Op::JumpIfFalse, &[Operand::Imm32(1), Operand::Register(30)]);
    code.push(Op::ReturnUndefined, &[]);
    code.push(Op::LoadInt32, &[Operand::Register(5), Operand::Imm32(0)]);
    // Ordinary source work makes the function profitable under the production
    // policy. It does not alter either allocation's effects or exception range.
    for index in 0..11u16 {
        code.push(
            Op::Add,
            &[
                Operand::Register(6 + 2 * index),
                Operand::Register(5),
                Operand::Register(2),
            ],
        );
        code.push(
            Op::Sub,
            &[
                Operand::Register(7 + 2 * index),
                Operand::Register(6 + 2 * index),
                Operand::Register(2),
            ],
        );
    }
    code.push(Op::ReturnValue, &[Operand::Register(3)]);
    let handler = code.next_pc();
    code.push(
        Op::Equal,
        &[
            Operand::Register(30),
            Operand::Register(0),
            Operand::Register(1),
        ],
    );
    let bad_alias = code.push(Op::JumpIfFalse, &[Operand::Imm32(1), Operand::Register(30)]);
    code.push(Op::ReturnValue, &[Operand::Register(3)]);
    let bad = code.push(Op::ReturnUndefined, &[]);
    assert_eq!(bad, bad_alias + 2);
    let code = code.finish();
    let positions = measure_wordcode_function(&code)
        .expect("source encoding")
        .instr_to_byte_pc;
    (
        BytecodeModule {
            module: "file:///allocation-group-recovery.js".into(),
            template_sites: Vec::new(),
            source_kind: SourceKind::JavaScript,
            functions: vec![
                Function {
                    id: 0,
                    name: "<main>".into(),
                    code: main.finish(),
                    ..Default::default()
                },
                Function {
                    id: 1,
                    name: "twoFixedObjects".into(),
                    param_count: 3,
                    scratch: 32,
                    is_strict: true,
                    handlers: vec![ExceptionHandler {
                        start: first,
                        end: second + 1,
                        target: handler,
                        exception: 29,
                    }],
                    code,
                    ..Default::default()
                },
            ],
            function_source: None,
            constants: Vec::new(),
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
        },
        positions,
    )
}
fn invoke(vm: &mut Interpreter, context: &ExecutionContext, fid: u32, child: Value) -> Value {
    vm.run_callable_sync(
        context,
        &Value::function(fid),
        Value::undefined(),
        smallvec![child, child, Value::number_i32(0)],
    )
    .expect("source catches the second allocation's OOM")
}
fn current(vm: &Interpreter, fid: u32) -> JitCodeGenerationSnapshot {
    let rows: Vec<_> = vm
        .jit_code_generation_snapshot()
        .into_iter()
        .filter(|g| {
            g.function_id == fid
                && g.tier == NativeFrameKind::Optimizing
                && g.lifecycle == CodeLifetimeState::Installed
                && g.linked
        })
        .collect();
    assert_eq!(rows.len(), 1, "one own current Graph entry: {rows:?}");
    rows.into_iter().next().unwrap()
}
fn assert_call_mapping(bundle: &JitArtifactBundle, generation: &JitCodeGenerationSnapshot) {
    assert_eq!(
        bundle.manifest().code_object_id(),
        generation.code_object_id
    );
    assert_eq!(bundle.manifest().function_id(), generation.function_id);
    assert!(
        generation.current_entry,
        "actual function cell selects this own Graph mapping"
    );
    let offset = generation
        .call_entry_offset
        .expect("actual private call capability");
    let code = bundle.file(JitArtifactFileName::Code).unwrap().contents();
    assert!((offset as usize) < code.len());
    assert_eq!(
        json(bundle, JitArtifactFileName::CodeMap)["callEntryOffset"].as_u64(),
        Some(u64::from(offset))
    );
}

fn assert_entered_group(report: &otter_vm::JitDebugReport, fid: u32, generation: u64) {
    // This callable enters through the actual private call entry. Its deopt
    // owner reports EnteredGenerationDeopt after validating the published
    // callee against the retained code cell. The separate VM/OSR entry owner
    // reports Bail; accepting that event would prove a different entry path.
    let entered: Vec<_> = report
        .events()
        .iter()
        .filter(|event| {
            matches!(event,
                JitDebugEvent::EnteredGenerationDeopt { callee_function_id, .. }
                if *callee_function_id == fid
            )
        })
        .collect();
    assert_eq!(
        entered.len(),
        1,
        "one exact current generation took the native group exit: {:?}",
        report.events()
    );
    assert!(matches!(
        entered[0],
        JitDebugEvent::EnteredGenerationDeopt {
            callee_code_object_id,
            callee_tier: JitDebugTier::Optimizing,
            callee_resume_pc: 1,
            exit_reason: otter_vm::native_abi::ExitReason::AllocationMiss,
            exit_action: otter_vm::native_abi::ExitAction::Resume,
            ..
        } if *callee_code_object_id == generation
    ));
    assert!(
        !report.events().iter().any(|event| matches!(
            event,
            JitDebugEvent::Bail { function_id, .. } if *function_id == fid
        )),
        "no own VM/OSR-entry exit: {:?}",
        report.events()
    );
    assert!(
        !report.events().iter().any(|event| matches!(
            event,
            JitDebugEvent::CompilePrepared { .. } | JitDebugEvent::InlineDeoptFrame { .. }
        )),
        "exact installed group continues without compilation or inline recovery: {:?}",
        report.events()
    );
}

fn json(bundle: &JitArtifactBundle, name: JitArtifactFileName) -> Json {
    serde_json::from_slice(
        bundle
            .file(name)
            .expect("actual emitted artifact")
            .contents(),
    )
    .unwrap()
}
fn assert_group(
    bundle: &JitArtifactBundle,
    first_byte_pc: u32,
    second_byte_pc: u32,
    comparison_byte_pc: u32,
) {
    let text = std::str::from_utf8(
        bundle
            .file(JitArtifactFileName::OptimizedIr)
            .unwrap()
            .contents(),
    )
    .unwrap();
    assert_eq!(
        text.lines()
            .filter(|line| line.contains(" = AllocationGroup("))
            .count(),
        1,
        "{text}"
    );
    assert_eq!(
        text.lines()
            .filter(|line| line.contains(" = AllocationProjection("))
            .count(),
        1,
        "{text}"
    );
    let map = json(bundle, JitArtifactFileName::CodeMap);
    let regions = map["regions"].as_array().unwrap();
    let own: Vec<_> = regions
        .iter()
        .filter(|r| {
            r["kind"] == "instruction" && r["functionId"] == bundle.manifest().function_id()
        })
        .collect();
    let group = own
        .iter()
        .find(|r| {
            r["operation"]
                .as_str()
                .is_some_and(|s| s.contains("AllocationGroup("))
        })
        .unwrap();
    let projection = own
        .iter()
        .find(|r| {
            r["operation"]
                .as_str()
                .is_some_and(|s| s.contains("AllocationProjection("))
        })
        .unwrap();
    assert_eq!(group["logicalPc"], 1);
    assert_eq!(group["bytePc"], first_byte_pc);
    assert_eq!(projection["logicalPc"], 2);
    assert_eq!(projection["bytePc"], second_byte_pc);
    assert!(group["startOffset"].as_u64().unwrap() < group["endOffset"].as_u64().unwrap());
    assert!(
        projection["startOffset"].as_u64().unwrap() < projection["endOffset"].as_u64().unwrap(),
        "the actual live second member has its own emitted projection"
    );
    let comparison = own
        .iter()
        .find(|r| {
            r["logicalPc"] == 3
                && r["operation"]
                    .as_str()
                    .is_some_and(|s| s.contains("StrictEqual { negate: false }"))
        })
        .expect("own emitted successful-source identity comparison");
    assert_eq!(comparison["bytePc"], comparison_byte_pc);
    assert!(
        comparison["startOffset"].as_u64().unwrap() < comparison["endOffset"].as_u64().unwrap()
    );
    let group_id = group["operationIndex"].as_u64().unwrap();
    let projection_id = projection["operationIndex"].as_u64().unwrap();
    let comparison_id = comparison["operationIndex"].as_u64().unwrap();
    let identity_ir = format!(
        "v{comparison_id} = StrictEqual {{ negate: false }} [{group_id}, {projection_id}] Tagged"
    );
    assert!(
        text.lines().any(|line| line.trim() == identity_ir),
        "the actual comparison reads both current group members: {identity_ir}\n{text}"
    );
    let branch = own
        .iter()
        .find(|r| {
            r["logicalPc"] == 4
                && r["operation"]
                    .as_str()
                    .is_some_and(|s| s.contains("Branch { kind: Truthy,"))
        })
        .expect("own source branch consumes the member comparison");
    let branch_id = branch["operationIndex"].as_u64().unwrap();
    assert!(
        text.lines().any(|line| {
            line.trim()
                .starts_with(&format!("v{branch_id} = Branch {{ kind: Truthy,"))
                && line.contains(&format!(" [{comparison_id}] None"))
        }),
        "the member comparison must remain a live source-control input: {text}"
    );
    let relocations = json(bundle, JitArtifactFileName::Relocations);
    assert!(
        relocations["relocations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["target"]["id"] == STUB_ALLOC_GROUP_ENSURE.id
                && r["target"]["signature"] == "allocValue3"
                && group["startOffset"].as_u64().unwrap() <= r["startOffset"].as_u64().unwrap()
                && r["startOffset"].as_u64().unwrap() < group["endOffset"].as_u64().unwrap()),
        "the exact current native group owns its real typed ensure edge"
    );
    let deopt = json(bundle, JitArtifactFileName::Deopt);
    let exit = deopt["exits"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["reason"] == "allocationMiss" && e["resumePcs"] == serde_json::json!([1]))
        .unwrap();
    assert_eq!(exit["action"], "resume");
    let state = deopt["frameStates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == exit["frameStateId"])
        .unwrap();
    let frames = state["frames"].as_array().unwrap();
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0]["functionId"], bundle.manifest().function_id());
    let slots = frames[0]["slots"].as_array().unwrap();
    for register in [0usize, 1] {
        assert_eq!(slots[register]["representation"], "tagged");
        assert_eq!(slots[register]["locationKind"], "stackSlot");
    }
    let points = json(bundle, JitArtifactFileName::Safepoints);
    let roots = points["records"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == u32::MAX - 1)
        .unwrap();
    let roots = roots["taggedLocations"].as_array().unwrap();
    for register in [0usize, 1] {
        let home = slots[register]["locationValue"]
            .as_str()
            .unwrap()
            .parse::<usize>()
            .unwrap();
        assert!(home.is_multiple_of(8) && home / 8 < roots.len());
        assert_eq!(roots[home / 8]["kind"], "spillSlot");
        assert_eq!(roots[home / 8]["index"], home / 8);
    }
}

#[derive(Debug)]
struct Tick {
    pc: u32,
    op: Op,
    first: u64,
    child: u64,
    aliases: bool,
}
struct Tracer {
    fid: u32,
    ticks: Arc<Mutex<Vec<Tick>>>,
}
impl StepTracer for Tracer {
    fn on_step(&mut self, event: &StepEvent<'_>) {
        if event.function_id != self.fid {
            return;
        }
        // No VM allocation or reentry: only copied scalar observations escape.
        let first = event
            .register_window
            .get(3)
            .map_or(Value::undefined().to_bits(), |v| v.to_bits());
        let aliases = event.register_window.get(0) == event.register_window.get(1);
        let child = event
            .register_window
            .get(0)
            .map_or(Value::undefined().to_bits(), |v| v.to_bits());
        self.ticks.lock().unwrap().push(Tick {
            pc: event.byte_pc,
            op: event.op,
            first,
            child,
            aliases,
        });
    }
}
fn page_state(offset: u32) -> (u32, usize, usize) {
    // SAFETY: caller supplies an actual live cell; its owning page is stable
    // throughout the noncollecting pressure setup and failed collector.
    let page = unsafe { &*page_base_from_offset(offset).cast::<PageHeader>() };
    (page.cage_offset, page.bump_remaining(), page.age_mark)
}
fn fill_page(vm: &mut Interpreter, offset: u32, leave: usize) -> u32 {
    let page = page_state(offset).0;
    let cell = otter_gc::header::HEADER_SIZE + std::mem::size_of::<PressureCell>();
    loop {
        // Publish the real LAB cursor before observing the owning page. A
        // native bump does not otherwise update PageHeader until retirement.
        let _ = vm.gc_heap().stats();
        let remaining = page_state(offset).1;
        assert!(remaining >= leave);
        if remaining == leave {
            break;
        }
        let mut chunk = (remaining - leave).min(LARGE_OBJECT_THRESHOLD);
        // Every admitted managed body needs a forwarding word. Leave enough
        // for that body rather than creating a fake header-only filler.
        if remaining - leave - chunk != 0 && remaining - leave - chunk < cell {
            chunk -= cell;
        }
        assert!(chunk >= cell && chunk.is_multiple_of(CELL_SIZE));
        let filler = vm
            .gc_heap_mut()
            .alloc_trailing_with_roots(PressureCell { word: 0xf111 }, chunk - cell, &mut |_| {})
            .expect("actual noncollecting nursery filler");
        assert_eq!(
            page_state(filler.offset()).0,
            page,
            "fill never advances prematurely"
        );
    }
    let _ = vm.gc_heap().stats();
    assert_eq!(page_state(offset).1, leave);
    page
}

fn assert_later_stress_recovers_first_source_with_current_homes(
    vm: &mut Interpreter,
    context: &ExecutionContext,
    fid: u32,
    positions: &[u32],
    generation: u64,
) {
    let fresh = NativeCtx::with_host_context(vm, NativeCallInfo::default_call(), None, |ctx| {
        ctx.scope(|mut scope| {
            let child = scope.object()?;
            Ok::<Value, NativeError>(scope.finish(child))
        })
    })
    .expect("fresh scoped input before policy change");
    let mut child = fresh.as_object().unwrap();
    let original = child.offset();
    let mut roots = RootScope::new(vm.gc_heap_mut());
    // SAFETY: this typed local and its scope stay live through the native
    // entry, restored source execution and all actual stress collections.
    unsafe {
        roots.add_raw_slot(std::ptr::addr_of_mut!(child).cast());
    }
    vm.gc_heap_mut().set_gc_stress(1, true);
    assert!(!vm.gc_heap().machine_allocation_allowed());
    let before = vm.jit_runtime_stats();
    let before_gc = vm.gc_heap_mut().gc_stats().clone();
    let ticks = Arc::new(Mutex::new(Vec::new()));
    vm.set_tracer(Some(Box::new(Tracer {
        fid,
        ticks: ticks.clone(),
    })));
    vm.begin_jit_debug_capture();
    let returned = invoke(vm, context, fid, Value::object(child));
    vm.set_tracer(None);
    let report = vm.take_jit_debug_report().unwrap();
    let after = vm.jit_runtime_stats();
    assert!(
        returned.is_object(),
        "both original source allocations complete under current stress policy"
    );
    assert_entered_group(&report, fid, generation);
    assert_eq!(after.optimized_deopts, before.optimized_deopts + 1);
    assert_eq!(
        current(vm, fid).code_object_id,
        generation,
        "policy recovery retains exact installed generation"
    );
    assert!(!report.truncated());
    assert_eq!(report.dropped_events(), 0);
    let ticks = ticks.lock().unwrap();
    assert!(!ticks.is_empty());
    assert_eq!((ticks[0].op, ticks[0].pc), (Op::NewObject, positions[1]));
    assert_eq!(
        ticks[0].first,
        Value::undefined().to_bits(),
        "no group cell was published before recovery"
    );
    let second = ticks
        .iter()
        .find(|t| t.pc == positions[2])
        .expect("actual second source follows the completed first");
    assert_ne!(
        second.first,
        Value::undefined().to_bits(),
        "the first canonical member completed before the second collecting allocation"
    );
    assert_eq!(
        ticks.last().unwrap().first,
        returned.to_bits(),
        "later source retains the GC-updated completed first member"
    );
    assert!(ticks.iter().all(|t| t.aliases));
    assert_eq!(
        ticks.last().unwrap().child,
        Value::object(child).to_bits(),
        "source and external root retain the same moving child"
    );
    assert_ne!(
        child.offset(),
        original,
        "canonical first-source stress really moves the live input"
    );
    assert!(vm.gc_heap_mut().gc_stats().minor_gc_cycles > before_gc.minor_gc_cycles);
    vm.gc_heap_mut().set_gc_stress(0, true);
    assert!(vm.gc_heap().machine_allocation_allowed());
}

#[test]
fn current_native_group_collector_failure_resumes_first_source_and_retains_its_completed_prefix() {
    init_cage_with_size(16 * 1024 * 1024).expect("this isolated binary owns its cage");
    let mut vm = Interpreter::new().expect("runtime bootstrap in the actual cage");
    vm.gc_heap_mut().set_gc_stress(0, true);
    vm.set_jit_debug_request(JitDebugRequest::artifacts().with_events(true));
    vm.begin_jit_debug_capture();
    let (module, positions) = source();
    let context = vm
        .link_module(module, otter_vm::source_registry::SourceRegistry::default())
        .expect("normal bytecode verifier admits source");
    let fid = context.main().id + 1;
    let calibration = Arc::new(Mutex::new(Vec::new()));
    vm.set_tracer(Some(Box::new(Tracer {
        fid,
        ticks: calibration.clone(),
    })));
    assert!(invoke(&mut vm, &context, fid, Value::undefined()).is_object());
    vm.set_tracer(None);
    let calibration = calibration.lock().unwrap();
    assert_eq!(
        calibration
            .iter()
            .filter(|tick| tick.op == Op::NewObject)
            .count(),
        2
    );
    assert!(calibration.iter().any(|tick| tick.pc == positions[1]));
    assert!(calibration.iter().any(|tick| tick.pc == positions[2]));
    assert!(!calibration.is_empty());
    drop(calibration);
    Arc::new(OtterJitCompiler::production_tiered()).install(&mut vm);
    let geometry = JitEmptyObjectAllocationPlan::new(0);
    let mut shape = 0;
    for _ in 0..16000 {
        let result = invoke(&mut vm, &context, fid, Value::undefined());
        let object = result.as_object().expect("warm source result");
        // SAFETY: freshly returned live object, authoritative VM shape offset.
        shape = unsafe {
            std::ptr::read_unaligned(
                otter_gc::cage_base()
                    .add(object.offset() as usize + geometry.shape_byte as usize)
                    .cast::<u32>(),
            )
        };
    }
    assert_ne!(shape, 0);
    let plan = JitEmptyObjectAllocationPlan::new(shape);
    let generation = current(&vm, fid);
    let artifacts = vm.take_jit_artifacts().expect("enabled actual artifacts");
    let bundle = artifacts
        .bundles()
        .iter()
        .find(|b| b.manifest().code_object_id() == generation.code_object_id)
        .expect("exact own current emitted generation");
    assert_group(bundle, positions[1], positions[2], positions[3]);
    assert_call_mapping(bundle, &generation);
    let _ = vm.take_jit_debug_report();
    // Graph specialized and Generic paths intentionally disable Template
    // source-prefix accounting. Calibrate the installed compiler's dispatch
    // hook on the actual short main, then observe this exact current callable.
    // The worker's full precompile trace, own cell/call mapping, observable
    // result and no own dispatch/exit leave no alternate executor. Later cold
    // probes also require this same generation's actual entered deopt event.
    let installed_calibration = Arc::new(Mutex::new(Vec::new()));
    vm.set_tracer(Some(Box::new(Tracer {
        fid: context.main().id,
        ticks: installed_calibration.clone(),
    })));
    let main = vm
        .run_callable_sync(
            &context,
            &Value::function(context.main().id),
            Value::undefined(),
            smallvec![],
        )
        .expect("ordinary short source under the installed policy");
    vm.set_tracer(None);
    assert!(main.is_undefined());
    let installed_calibration = installed_calibration.lock().unwrap();
    assert_eq!(installed_calibration.len(), 1);
    assert_eq!(installed_calibration[0].op, Op::ReturnUndefined);
    assert_eq!(installed_calibration[0].pc, 0);
    drop(installed_calibration);
    assert_eq!(current(&vm, fid).code_object_id, generation.code_object_id);
    let fit_ticks = Arc::new(Mutex::new(Vec::new()));
    vm.set_tracer(Some(Box::new(Tracer {
        fid,
        ticks: fit_ticks.clone(),
    })));
    vm.begin_jit_debug_capture();
    assert!(invoke(&mut vm, &context, fid, Value::undefined()).is_object());
    vm.set_tracer(None);
    let fit_report = vm.take_jit_debug_report().unwrap();
    assert!(!fit_report.truncated());
    assert_eq!(fit_report.dropped_events(), 0);
    assert!(
        fit_ticks.lock().unwrap().is_empty(),
        "complete calibrated own dispatch hook"
    );
    assert!(
        !fit_report.events().iter().any(|event| matches!(
            event,
            JitDebugEvent::CompilePrepared { .. }
                | JitDebugEvent::Bail { .. }
                | JitDebugEvent::EnteredGenerationDeopt { .. }
                | JitDebugEvent::InlineDeoptFrame { .. }
        )),
        "same own installed body: {:?}",
        fit_report.events()
    );
    assert_eq!(current(&vm, fid).code_object_id, generation.code_object_id);
    assert!(current(&vm, fid).current_entry);
    assert_later_stress_recovers_first_source_with_current_homes(
        &mut vm,
        &context,
        fid,
        &positions,
        generation.code_object_id,
    );
    // Reap actual warm garbage and standby pages before pressure. The tested
    // input is then freshly allocated and survives one real collecting cycle.
    vm.force_gc().expect("settle warm heap");
    vm.force_gc().expect("settle old sweep");
    let fresh =
        NativeCtx::with_host_context(&mut vm, NativeCallInfo::default_call(), None, |ctx| {
            ctx.scope(|mut scope| {
                let child = scope.object()?;
                Ok::<Value, NativeError>(scope.finish(child))
            })
        })
        .expect("handle-scoped live input");
    let mut child = fresh.as_object().unwrap();
    let young = child.offset();
    let mut roots = RootScope::new(vm.gc_heap_mut());
    // SAFETY: this engine fixture's compressed typed slot stays stationary
    // until the registered scope drops; contributors do not use this API.
    unsafe {
        roots.add_raw_slot(std::ptr::addr_of_mut!(child).cast());
    }
    vm.force_gc()
        .expect("actual first survival moves the young input");
    assert_ne!(child.offset(), young);
    let aged = child.offset();
    assert!(page_state(aged).2 > otter_gc::page::PAGE_HEADER_SIZE);
    // The heap's handle arena owns all pressure bodies even if a later
    // successful collector becomes reachable; pressure is retained, not RSS.
    // SAFETY: this actual heap and handle stack outlive every local below.
    let pressure_scope = unsafe { HandleScope::from_ptr(vm.gc_heap_mut().handle_stack_ptr()) };
    let mut large = Vec::new();
    loop {
        match vm.gc_heap_mut().alloc_trailing_with_roots(
            PressureCell { word: 0x105 },
            200 * 1024,
            &mut |_| {},
        ) {
            Ok(value) => large.push(pressure_scope.local(value)),
            Err(OutOfMemory::CageExhausted) => break,
            Err(error) => panic!("unexpected actual pressure failure: {error:?}"),
        }
    }
    assert!(!large.is_empty());
    // A multi-page LOS request can fail with 1..3 pages still free. Consume
    // those pages with actual one-page large cells before requiring zero.
    let cell = otter_gc::header::HEADER_SIZE + std::mem::size_of::<PressureCell>();
    while otter_gc::cage_stats().unwrap().free_pages != 0 {
        let value = vm
            .gc_heap_mut()
            .alloc_trailing_with_roots(
                PressureCell { word: 0x105 },
                LARGE_OBJECT_THRESHOLD + CELL_SIZE - cell,
                &mut |_| {},
            )
            .expect("real residual cage page pressure");
        large.push(pressure_scope.local(value));
    }
    assert_eq!(otter_gc::cage_stats().unwrap().free_pages, 0);
    let no_gc = vm.gc_heap_mut().always_allocate_scope();
    let mut last_page = fill_page(&mut vm, aged, 0);
    let mut tail_offset = aged;
    for index in 1..otter_gc::space::DEFAULT_NEW_SPACE_PAGES {
        let first = vm
            .gc_heap_mut()
            .alloc(PressureCell { word: index as u64 })
            .expect("next existing nursery page");
        let page = page_state(first.offset()).0;
        assert_ne!(page, last_page);
        let leave = if index + 1 == otter_gc::space::DEFAULT_NEW_SPACE_PAGES {
            plan.cell_bytes as usize
        } else {
            0
        };
        last_page = fill_page(&mut vm, first.offset(), leave);
        tail_offset = first.offset();
    }
    drop(no_gc);
    assert_eq!(
        page_state(tail_offset).1,
        plan.cell_bytes as usize,
        "exactly the first canonical cell fits"
    );
    let prefix_offset =
        (last_page as usize + otter_gc::PAGE_SIZE - plan.cell_bytes as usize) as u32;
    let window = vm.gc_heap_mut().machine_allocation_window();
    // The native group requires both fixed cells at once. The exact one-cell
    // LAB tail is deliberately preserved for the first canonical source after
    // its failed group refill; an empty LAB would contradict that prefix.
    assert!(vm.gc_heap().machine_allocation_allowed());
    // SAFETY: read-only inspection of the actual stable heap LAB descriptor.
    let remaining = unsafe { (*window.lab).remaining() };
    assert_eq!(
        remaining, plan.cell_bytes as usize,
        "exact one-cell native LAB tail"
    );
    let group_bytes = (plan.cell_bytes as usize).checked_mul(2).unwrap();
    assert!(
        remaining < group_bytes,
        "both original fixed members cannot fit"
    );
    assert!(
        !vm.gc_heap()
            .oom_flag()
            .load(std::sync::atomic::Ordering::Relaxed)
    );
    let before = vm.jit_runtime_stats();
    let before_gc = vm.gc_heap_mut().gc_stats().clone();
    let ticks = Arc::new(Mutex::new(Vec::new()));
    vm.set_tracer(Some(Box::new(Tracer {
        fid,
        ticks: ticks.clone(),
    })));
    vm.begin_jit_debug_capture();
    vm.gc_heap_mut().start_gc_pause_capture(64).unwrap();
    let returned = invoke(&mut vm, &context, fid, Value::object(child));
    vm.set_tracer(None);
    let capture = vm.gc_heap_mut().take_gc_pause_capture().unwrap();
    let report = vm.take_jit_debug_report().unwrap();
    vm.gc_heap()
        .read_payload(large[0].get(), |body| assert_eq!(body.word, 0x105));
    let after = vm.jit_runtime_stats();
    // Only the current native group's typed ensure runs in this interval;
    // resumed source allocation uses the ordinary VM allocator. Require its
    // actual collector refusal, not merely a LAB guard or policy miss.
    assert_eq!(
        after.alloc_value_stub_out_of_memory,
        before.alloc_value_stub_out_of_memory + 1,
        "one real group ensure collector refusal"
    );
    assert_eq!(after.alloc_value_stub_ok, before.alloc_value_stub_ok);
    assert_eq!(after.alloc_value_stub_miss, before.alloc_value_stub_miss);
    assert_eq!(after.alloc_value_stub_other, before.alloc_value_stub_other);
    assert_entered_group(&report, fid, generation.code_object_id);
    assert_eq!(
        after.optimized_deopts,
        before.optimized_deopts + 1,
        "one first-source eager recovery"
    );
    assert_eq!(current(&vm, fid).code_object_id, generation.code_object_id);
    assert!(!report.truncated());
    assert_eq!(report.dropped_events(), 0);
    assert!(!capture.incomplete);
    assert_eq!(capture.dropped_records, 0);
    assert!(capture.records.len() >= 2);
    for record in &capture.records[..2] {
        assert_eq!(record.kind, GcPauseKind::Minor);
        assert_eq!(record.trigger, GcPauseTrigger::NurseryCapacity);
        assert_eq!(record.outcome, GcPauseOutcome::CollectionFailed);
        assert_eq!(record.minor_cycles_before, record.minor_cycles_after);
    }
    assert_eq!(
        vm.gc_heap_mut().gc_stats().minor_gc_cycles,
        before_gc.minor_gc_cycles
    );
    assert_eq!(
        child.offset(),
        aged,
        "actual preflight failure cannot forward the live input"
    );
    // SAFETY: failed promotion leaves the typed live root/header untouched.
    assert!(!unsafe { (*child.as_header_ptr()).is_forwarded() });
    let ticks = ticks.lock().unwrap();
    assert!(!ticks.is_empty(), "complete resumed dispatch was observed");
    assert_eq!(
        (ticks[0].op, ticks[0].pc),
        (Op::NewObject, positions[1]),
        "resume FIRST exact encoded source PC"
    );
    assert_eq!(
        ticks[0].first,
        Value::undefined().to_bits(),
        "no phantom completed group output"
    );
    let second = ticks
        .iter()
        .find(|t| t.pc == positions[2])
        .expect("second original source actually dispatched");
    assert_eq!(second.op, Op::NewObject);
    assert!(
        ticks.iter().all(|t| t.aliases),
        "restored canonical child aliases"
    );
    assert_eq!(
        second.first,
        returned.to_bits(),
        "catch retains the already completed first source object"
    );
    let object = returned
        .as_object()
        .expect("first source succeeds before second source OOM");
    assert_eq!(
        object.offset(),
        prefix_offset,
        "canonical first uses preserved real nursery tail"
    );
    // SAFETY: returned first cell is still live and no allocation intervenes;
    // the plan comes from the sole VM geometry, not copied body constants.
    unsafe {
        let base = otter_gc::cage_base().add(object.offset() as usize);
        assert_eq!(
            std::ptr::read_unaligned(base.cast::<u64>()),
            plan.header_word
        );
        assert_eq!(
            std::ptr::read_unaligned(base.add(plan.shape_byte as usize).cast::<u32>()),
            shape
        );
        for byte in plan.initial_value_bytes {
            assert_eq!(
                std::ptr::read_unaligned(base.add(byte as usize).cast::<u64>()),
                Value::undefined().to_bits()
            );
        }
    }
    assert!(
        ticks
            .iter()
            .any(|t| t.op == Op::ReturnValue && t.first == returned.to_bits()),
        "actual handler returns the retained prefix"
    );
    assert!(
        !large.is_empty(),
        "all cage pressure remained managed until terminal completion"
    );
}
