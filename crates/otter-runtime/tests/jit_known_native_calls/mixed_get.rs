//! Completed Get failures through one installed source operation.
//!
//! # Contents
//! - Real Error-materialization OOM from Proxy Get and ordinary getters.
//! - Iterator mapper failure before observable throw-Close or source handlers.
//! - Native current-generation, source/root, moving child and emitted C-call joins.
//!
//! # Invariants
//! - Normal source execution admits the exact property consumer in each tier.
//! - The property operation uses its existing committed C entry and published
//!   source/root record; no synthetic JS return-site is required for that entry.
//! - Native callbacks retain only owned records and never assert or unwind.
//! - The same callback performs real collection before refusing its Error build.
//! - Fresh local allocation OOM still enters the ordinary source handler once.
//!
//! # See also
//! - `super::completion` provides the real completed allocator failure.
//! - `otter_vm::native_abi::CommittedValueError` preserves the producer domain.

use super::*;
use otter_runtime::OtterError;

const CAP: u64 = 4 * 1024 * 1024;
const GET_MODULE: &str = "mixed-get-setup.js";
const SUBJECT: &str = "mixedGetValue";
const SOURCE_LINE: &str = "function mixedGetValue(value) { return value.payload; }";
const DEFINITIONS: &str = r#"
function mixedGetValue(value) { return value.payload; }
const mixedGetWarm = new Proxy({payload:719},{get(target,key,receiver){return Reflect.get(target,key,receiver);}});
const mixedGetEffects = [0,0,0,0,0,0];
const mixedGetNoTrapProto = new Proxy({get payload(){if(this!==mixedGetInput3)throw 997;return nativeKindCompletion(nativeKindRenew(719));}},{get get(){mixedGetEffects[3]++;mixedGetEffects[4]=motionChildOffset(mixedGetInput3);motionCollect();mixedGetEffects[5]=motionChildOffset(mixedGetInput3);return undefined;}});
// Prepare the exact immutable prototype root before the measured young receiver.
Object.create(mixedGetNoTrapProto);
"#;
const WARM: &str = "for(let mixedWarm=0;mixedWarm<1000;mixedWarm++){if(mixedGetValue(mixedGetWarm)!==719)throw 997;}";

fn capture(fixture: &mut Fixture, source: &str, module: &str) {
    let output = fixture.run(source, module);
    let artifacts = output.jit_artifacts().unwrap();
    assert!(!artifacts.truncated());
    let report = output.jit_debug_report().unwrap();
    assert!(!report.truncated());
    assert_eq!(report.dropped_events(), 0);
    fixture.events.extend(report.events().iter().cloned());
    for bundle in artifacts.bundles() {
        fixture
            .artifacts
            .insert(bundle.manifest().code_object_id(), bundle.clone());
    }
}

fn admit(fixture: &mut Fixture) -> Option<JitCodeGenerationSnapshot> {
    capture(fixture, &format!("{DEFINITIONS}\n{WARM}"), GET_MODULE);
    if fixture.selection == JitSelection::InterpreterOnly {
        assert!(fixture.current_in_module(SUBJECT, GET_MODULE).is_none());
        return None;
    }
    for batch in 0..=128 {
        if let Some(own) = fixture.current_in_module(SUBJECT, GET_MODULE) {
            assert!(own.current_entry && own.linked && own.call_entry_offset.is_some());
            return Some(own);
        }
        assert!(
            batch < 128,
            "normal own Get admission: generations={:?}; artifacts={:?}; events={:?}",
            fixture.runtime.jit_code_generation_snapshot(),
            fixture
                .artifacts
                .values()
                .filter(|bundle| bundle.manifest().module() == GET_MODULE)
                .map(|bundle| bundle.manifest())
                .collect::<Vec<_>>(),
            fixture.events.iter().rev().take(32).collect::<Vec<_>>()
        );
        capture(fixture, WARM, &format!("mixed-get-warm-{batch}.js"));
    }
    unreachable!()
}

fn assert_get_entry(bundle: &JitArtifactBundle, own: &JitCodeGenerationSnapshot) {
    let bytecode = std::str::from_utf8(
        bundle
            .file(JitArtifactFileName::Bytecode)
            .unwrap()
            .contents(),
    )
    .unwrap();
    assert_eq!(bytecode.lines().next(), Some("; otter bytecode"));
    assert!(
        bytecode
            .lines()
            .nth(1)
            .unwrap()
            .starts_with(&format!("; function={} ", own.function_id))
    );
    let operations: Vec<_> = bytecode
        .lines()
        .skip(2)
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pc = fields.next()?.parse::<u32>().ok()?;
            let byte_pc = fields.next()?.strip_prefix("byte=")?.parse::<u32>().ok()?;
            (fields.next()? == "LoadProperty").then_some((pc, byte_pc))
        })
        .collect();
    assert_eq!(operations.len(), 1, "one actual source Get");
    let (pc, byte_pc) = operations[0];
    let map = json(bundle, JitArtifactFileName::CodeMap);
    assert_eq!(
        map["callEntryOffset"].as_u64(),
        own.call_entry_offset.map(u64::from)
    );
    let regions: Vec<_> = map["regions"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|region| {
            region["kind"] == "instruction"
                && region["functionId"].as_u64() == Some(u64::from(own.function_id))
                && region["logicalPc"].as_u64() == Some(u64::from(pc))
                && region["bytePc"].as_u64() == Some(u64::from(byte_pc))
                && region["operation"]
                    .as_str()
                    .is_some_and(|op| op.starts_with("LoadProperty {") || op.contains(" Generic {"))
        })
        .collect();
    assert_eq!(regions.len(), 1, "one actual emitted property consumer");
    let region = regions[0];
    let start = region["startOffset"].as_u64().unwrap();
    let end = region["endOffset"].as_u64().unwrap();
    assert!(start < end);
    let relocations = json(bundle, JitArtifactFileName::Relocations);
    let links: Vec<_> = relocations["relocations"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|link| {
            link["target"]["kind"] == "runtimeStub"
                && link["target"]["id"].as_u64() == Some(u64::from(abi::STUB_JIT_LOAD_PROPERTY.id))
                && link["target"]["name"] == abi::runtime_stub_name(abi::STUB_JIT_LOAD_PROPERTY.id)
                && link["startOffset"]
                    .as_u64()
                    .is_some_and(|offset| start <= offset && offset < end)
        })
        .collect();
    assert_eq!(links.len(), 1, "one source-owned committed Get edge");
    let link_end = links[0]["endOffset"].as_u64().unwrap() as usize;
    let code = bundle.file(JitArtifactFileName::Code).unwrap().contents();
    #[cfg(target_arch = "aarch64")]
    assert_eq!(
        code.get(link_end..link_end + 4),
        Some([0x00, 0x02, 0x3f, 0xd6].as_slice())
    );
    #[cfg(target_arch = "x86_64")]
    {
        use yaxpeax_arch::LengthedInstruction;
        use yaxpeax_x86::amd64::{InstDecoder, Opcode};
        let decoder = InstDecoder::default();
        let mut cursor = link_end;
        loop {
            let instruction = decoder.decode_slice(&code[cursor..end as usize]).unwrap();
            let next = cursor + instruction.len().to_const() as usize;
            assert!(cursor < next && next <= end as usize);
            if instruction.opcode() == Opcode::CALL {
                assert_eq!(&code[cursor..next], &[0x41, 0xff, 0xd3]);
                break;
            }
            assert!(!matches!(
                instruction.opcode(),
                Opcode::JMP | Opcode::RETURN
            ));
            cursor = next;
        }
    }
    let roots = json(bundle, JitArtifactFileName::Safepoints);
    let records: Vec<_> = roots["records"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|record| record["callPc"].as_u64() == Some(u64::from(pc)))
        .collect();
    assert_eq!(records.len(), 1, "exact Get source/root record");
    let tagged = records[0]["taggedLocations"].as_array().unwrap();
    assert!(!tagged.is_empty(), "actual boxed receiver remains rooted");
    assert!(
        tagged
            .iter()
            .all(|location| location["kind"] != "machineRegister")
    );
    assert!(
        roots["returnSites"].as_array().unwrap().is_empty(),
        "a C Get transition does not fabricate a JS return anchor"
    );
}

#[test]
fn completed_get_oom_never_runs_source_handlers_or_throw_close() {
    for (selection, extra) in [
        (JitSelection::InterpreterOnly, false),
        (JitSelection::Template, false),
        (JitSelection::ProductionTiered, false),
        (JitSelection::ProductionTiered, true),
    ] {
        let mut fixture = Fixture::new_capped(selection, extra, Some(CAP));
        let own = admit(&mut fixture);
        if let Some(own) = &own {
            assert_get_entry(&fixture.artifacts[&own.code_object_id], own);
        }
        for route in 0..4 {
            fixture.records.lock().unwrap().clear();
            *fixture.trace.lock().unwrap() = Trace::default();
            let receiver = if route == 3 {
                "Object.create(mixedGetNoTrapProto)".to_string()
            } else if route == 1 {
                format!("{{get payload(){{return nativeKindCompletion(mixedGetChild{route});}}}}")
            } else {
                format!(
                    "new Proxy({{}},{{get(target,key,receiver){{return nativeKindCompletion(mixedGetChild{route});}}}})"
                )
            };
            let action = if route == 2 {
                format!(
                    "const input={{index:0,next(){{return this.index++?{{done:true}}:{{done:false,value:mixedGetInput{route}}};}},return(){{mixedGetEffects[2]++;return{{done:true}};}},[Symbol.iterator](){{return this;}}}};Iterator.from(input).map(mixedGetValue).toArray();"
                )
            } else {
                format!("mixedGetValue(mixedGetInput{route});")
            };
            let source = format!(
                "const mixedGetChild{route}=nativeKindRenew(719);const mixedGetInput{route}={receiver};try{{{action}}}catch(error){{mixedGetEffects[0]++;}}finally{{mixedGetEffects[1]++;}}41;"
            );
            let before = fixture.runtime.execution_stats();
            fixture.trace.lock().unwrap().recording = true;
            let result = completion::execute(
                &mut fixture,
                &source,
                &format!("mixed-get-failure-{route}.js"),
            );
            fixture.trace.lock().unwrap().recording = false;
            let error = result.expect_err(
                "completed Error-building OOM cannot reenter source catch/finally/Close",
            );
            assert!(
                matches!(error,OtterError::OutOfMemory{requested_bytes,heap_limit_bytes} if requested_bytes>CAP && heap_limit_bytes==CAP),
                "{error:?}"
            );
            assert!(fixture.runtime.execution_stats().gc_cycles > before.gc_cycles);
            let trace = fixture.trace.lock().unwrap();
            assert!(trace.total > 0);
            assert_eq!(trace.total, trace.counts.values().sum::<usize>());
            if let Some(own) = &own {
                assert_eq!(
                    trace.counts.get(&own.function_id).copied().unwrap_or(0),
                    0,
                    "no source replay of the exact native Get subject"
                );
                let current = fixture.current_in_module(SUBJECT, GET_MODULE).unwrap();
                assert_eq!(current.code_object_id, own.code_object_id);
                assert_eq!(current.generated_deopts, own.generated_deopts);
                assert_eq!(current.active_count, 0);
            }
            drop(trace);
            let records = fixture.records.lock().unwrap();
            assert_eq!(records.len(), 1, "one actual callback without replay");
            let record = records[0]
                .as_ref()
                .unwrap_or_else(|error| panic!("{error}"));
            let [child] = record.children.as_slice() else {
                panic!("one exact actual rooted child")
            };
            assert_eq!(child.marker_before, 719.0);
            assert_eq!(child.marker_after, 719.0);
            assert_ne!(
                child.before, child.after,
                "real argument evacuation before failure"
            );
            if let Some(own) = &own {
                completion::assert_observed_source(
                    &record.before,
                    &record.generations_before,
                    own,
                    SUBJECT,
                    SOURCE_LINE,
                    GET_MODULE,
                );
                completion::assert_observed_source(
                    &record.after,
                    &record.generations_after,
                    own,
                    SUBJECT,
                    SOURCE_LINE,
                    GET_MODULE,
                );
            }
            drop(records);
            let effects = fixture.run("JSON.stringify(mixedGetEffects);", "mixed-get-effects.js");
            let effects: Json = serde_json::from_str(effects.completion_string()).unwrap();
            assert_eq!(effects.as_array().unwrap().len(), 6);
            assert_eq!(effects[0], 0, "no source catch after terminal Get");
            assert_eq!(effects[1], 0, "no source finally after terminal Get");
            assert_eq!(effects[2], 0, "no throw-Close after terminal mapper");
            assert_eq!(effects[3], if route == 3 { 1 } else { 0 });
            if route == 3 {
                let before = effects[4].as_u64().unwrap();
                let after = effects[5].as_u64().unwrap();
                assert_ne!(
                    before, after,
                    "the actual ordinary receiver moves during NoTrap lookup"
                );
            } else {
                assert_eq!(effects[4], 0);
                assert_eq!(effects[5], 0);
            }
            fixture.run(
                "mixedGetEffects[3]=0;mixedGetEffects[4]=0;mixedGetEffects[5]=0;",
                "mixed-get-reset-effects.js",
            );
        }
        let local=fixture.run("let mixedGetLocalCaught=0;let mixedGetLocalResult;try{'x'.repeat(8*1024*1024);}catch(error){mixedGetLocalCaught++;mixedGetLocalResult=[error.name,error.message.includes('4194304'),Object.getPrototypeOf(error)===RangeError.prototype];}JSON.stringify([mixedGetLocalResult,mixedGetLocalCaught]);","mixed-get-local-allocation.js");
        assert_eq!(
            local.completion_string(),
            "[[\"RangeError\",true,true],1]",
            "fresh local allocation keeps source catchability"
        );
    }
}
