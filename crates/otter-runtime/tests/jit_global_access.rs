//! Native binding hits and committed misses on both machine targets.
//!
//! # Contents
//! - Own Template OSR code for ordinary globals and observable accessors.
//! - Permanent lexical cells, const/TDZ errors, and guarded dictionary slots.
//! - Lexical shadowing and descriptor changes through the same native body.
//! - Frozen ordinary and dictionary globals retire guarded stores in both tiers.
//! - Sealed dictionary globals retain writable committed stores.
//! - Safely exchanged foreign callables retain source-realm binding semantics.
//! - Young-child motion after native lexical/global writes at stress strides 1–16.
//!
//! # Invariants
//! - Fixtures earn native admission through normal source execution.
//! - Each measured call enters its exact installed generation; an interpreter
//!   trace, side exit, recompilation, or another function's bundle cannot pass.
//! - Read hits have no calls; a store hit owns only the canonical NoGC barrier.
//!   Each cold region owns one committed binding call.
//! - Observable getters, setters and argument evaluation execute exactly once.
//! - Collecting callbacks retain only scoped handles and owned motion records;
//!   all fixture assertions run after returning from the native callback.
//!
//! # See also
//! - `jit_artifacts` joins symbolic binding cells to exact code offsets.
//! - `support/moving_children` owns panic-free child motion observations.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use otter_runtime::{
    JitArtifactBundle, JitArtifactFileName, JitDebugEvent, JitDebugRequest, JitDebugTarget,
    JitDebugTier, JitSelection, Runtime, RuntimeExtensionInstaller, RuntimeNativeCall,
    RuntimeNativeCtx, RuntimeNativeError, RuntimeValue, SourceInput,
    inspect::{StepEvent, StepTracer},
};
use otter_vm::{
    JitCodeGenerationSnapshot,
    native_abi::{CodeLifetimeState, NativeFrameKind},
};

#[path = "support/moving_children.rs"]
mod moving_children;
#[cfg(target_arch = "x86_64")]
#[path = "support/native_code.rs"]
#[allow(dead_code)]
mod native_code;

#[path = "jit_global_access/foreign_realms.rs"]
mod foreign_realms;

const MODULE: &str = "native-binding-setup.js";

fn json(bundle: &JitArtifactBundle, file: JitArtifactFileName) -> serde_json::Value {
    serde_json::from_slice(bundle.file(file).unwrap().contents()).unwrap()
}

fn calls(code: &[u8], start: usize, end: usize) -> usize {
    #[cfg(target_arch = "aarch64")]
    {
        assert_eq!(start % 4, 0);
        assert_eq!(end % 4, 0);
        code[start..end]
            .chunks_exact(4)
            .filter(|bytes| {
                let word = u32::from_le_bytes((*bytes).try_into().unwrap());
                word & 0xfffffc1f == 0xd63f0000 || word & 0xfc000000 == 0x94000000
            })
            .count()
    }
    #[cfg(target_arch = "x86_64")]
    {
        native_code::decode(code, start, end)
            .iter()
            .filter(|instruction| instruction.opcode() == yaxpeax_x86::amd64::Opcode::CALL)
            .count()
    }
}

fn binding_bytecode(bundle: &JitArtifactBundle) -> BTreeMap<u32, String> {
    let text = std::str::from_utf8(
        bundle
            .file(JitArtifactFileName::Bytecode)
            .unwrap()
            .contents(),
    )
    .unwrap();
    let mut lines = text.lines();
    assert_eq!(lines.next(), Some("; otter bytecode"));
    assert!(
        lines
            .next()
            .unwrap()
            .starts_with(&format!("; function={} ", bundle.manifest().function_id()))
    );
    lines
        .map(|line| {
            let mut fields = line.split_whitespace();
            let _logical_pc = fields.next().unwrap().parse::<u32>().unwrap();
            let byte_pc = fields
                .next()
                .unwrap()
                .strip_prefix("byte=")
                .unwrap()
                .parse::<u32>()
                .unwrap();
            (byte_pc, fields.next().unwrap().to_owned())
        })
        .collect()
}

fn assert_binding_regions(bundle: &JitArtifactBundle, hit: bool, lexical: bool) {
    assert_eq!(bundle.manifest().architecture(), std::env::consts::ARCH);
    assert_eq!(bundle.manifest().tier(), JitDebugTier::Template);
    let code = bundle.file(JitArtifactFileName::Code).unwrap().contents();
    assert_eq!(code.len() as u64, bundle.manifest().code_bytes());
    let map = json(bundle, JitArtifactFileName::CodeMap);
    let regions = map["regions"].as_array().unwrap();
    let hits = regions
        .iter()
        .filter(|region| region["kind"] == "templateBindingHit")
        .collect::<Vec<_>>();
    assert!(!hits.is_empty(), "own binding operation regions");
    assert_eq!(
        hits.iter()
            .any(|region| region["startOffset"] != region["endOffset"]),
        hit,
        "the requested binding hit is physically emitted"
    );
    let relocations = json(bundle, JitArtifactFileName::Relocations);
    let relocations = relocations["relocations"].as_array().unwrap();
    let bytecode = binding_bytecode(bundle);
    for region in hits {
        let byte_pc = u32::try_from(region["bytePc"].as_u64().unwrap()).unwrap();
        let opcode = bytecode[&byte_pc].as_str();
        let start = region["startOffset"].as_u64().unwrap() as usize;
        let end = region["endOffset"].as_u64().unwrap() as usize;
        let barrier = start < end && matches!(opcode, "StoreGlobalBinding" | "StoreGlobalChecked");
        assert_eq!(
            calls(code, start, end),
            usize::from(barrier),
            "{opcode} hit contains only its canonical NoGC barrier call"
        );
        let hit_calls = relocations
            .iter()
            .filter(|relocation| {
                relocation["target"]["kind"] == "runtimeStub"
                    && relocation["startOffset"].as_u64().unwrap() >= start as u64
                    && relocation["endOffset"].as_u64().unwrap() <= end as u64
            })
            .collect::<Vec<_>>();
        assert_eq!(hit_calls.len(), usize::from(barrier));
        if barrier {
            assert_eq!(hit_calls[0]["target"]["name"], "write_barrier");
            assert_eq!(hit_calls[0]["target"]["id"], 26);
        }
        assert!(
            matches!(
                opcode,
                "LoadGlobalThis"
                    | "LoadGlobalOrThrow"
                    | "LoadGlobalOrUndefined"
                    | "GlobalBindingExists"
                    | "StoreGlobalBinding"
                    | "StoreGlobalChecked"
            ),
            "exact own canonical binding opcode: {opcode}"
        );
        let cold = regions
            .iter()
            .find(|candidate| {
                candidate["kind"] == "templateBindingCold"
                    && candidate["bytePc"] == region["bytePc"]
            })
            .unwrap();
        let start = cold["startOffset"].as_u64().unwrap() as usize;
        let end = cold["endOffset"].as_u64().unwrap() as usize;
        if start < end {
            assert_eq!(calls(code, start, end), 1, "one committed cold operation");
            assert_eq!(
                relocations
                    .iter()
                    .filter(|relocation| {
                        relocation["target"]["kind"] == "runtimeStub"
                            && relocation["target"]["name"] == "jit_binding_value"
                            && relocation["startOffset"].as_u64().unwrap() >= start as u64
                            && relocation["endOffset"].as_u64().unwrap() <= end as u64
                    })
                    .count(),
                1,
                "cold call owns its exact binding descriptor"
            );
        }
    }
    if lexical {
        assert!(
            relocations.iter().any(|relocation| {
                relocation["target"]["kind"] == "globalLexicalCell"
                    && relocation["target"]["functionId"].as_u64()
                        == Some(u64::from(bundle.manifest().function_id()))
                    && regions.iter().any(|region| {
                        region["kind"] == "templateBindingGuard"
                            && region["bytePc"] == relocation["target"]["bytePc"]
                            && region["startOffset"].as_u64().unwrap()
                                <= relocation["startOffset"].as_u64().unwrap()
                            && relocation["endOffset"].as_u64().unwrap()
                                <= region["endOffset"].as_u64().unwrap()
                    })
            }),
            "permanent lexical cell belongs to this function's emitted guard"
        );
    }
}

fn assert_strict_global_store_operations(bundle: &JitArtifactBundle) {
    let bytecode = binding_bytecode(bundle);
    let operations = bytecode
        .iter()
        .filter(|(_, opcode)| {
            matches!(
                opcode.as_str(),
                "GlobalBindingExists" | "StoreGlobalChecked"
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(operations.len(), 2);
    assert_eq!(operations[0].1, "GlobalBindingExists");
    assert_eq!(operations[1].1, "StoreGlobalChecked");
    let map = json(bundle, JitArtifactFileName::CodeMap);
    let regions = map["regions"].as_array().unwrap();
    let cold = regions
        .iter()
        .filter(|region| region["kind"] == "templateBindingCold")
        .collect::<Vec<_>>();
    assert_eq!(cold.len(), 2);
    for (byte_pc, opcode) in operations {
        let matching = cold
            .iter()
            .filter(|region| region["bytePc"].as_u64() == Some(u64::from(*byte_pc)))
            .collect::<Vec<_>>();
        assert_eq!(matching.len(), 1, "own {opcode} committed region");
        assert!(matching[0]["startOffset"] != matching[0]["endOffset"]);
    }
}

/// `TST w16, #1`: the dictionary bit of the global's ShapeState.
#[cfg(target_arch = "aarch64")]
const DICTIONARY_STATE_TEST: u32 = 0x7200_021f;

/// The dictionary discriminator is part of this exact emitted store guard.
/// Combined with a zero-cold measured hit, it proves the actual global used
/// dictionary storage and passed the emitter's nonprototype store admission.
fn assert_dictionary_store_guard(code: &[u8], start: usize, end: usize) {
    #[cfg(target_arch = "aarch64")]
    {
        let words = code[start..end]
            .chunks_exact(4)
            .map(|bytes| u32::from_le_bytes(bytes.try_into().unwrap()))
            .collect::<Vec<_>>();
        assert!(
            words.windows(2).any(|window| {
                window[0] == DICTIONARY_STATE_TEST && window[1] & 0xff00_001f == 0x5400_0000
            }),
            "own dictionary ShapeState test: TST w16, #1 ; B.EQ miss"
        );
        // MOV w17, #0xf8 then TST w16, w17: the five bits excluding an
        // eligible ordinary global lookup (opaque facts plus provisional
        // state) precede B.NE miss.
        let reject_mask = words.iter().position(|word| {
            word & 0x7fe0_001f == 0x5280_0011
                && (word >> 5) & 0xffff
                    == u32::from(
                        otter_vm::object::ShapeState::OPAQUE_LOOKUP_MASK
                            | otter_vm::object::ShapeState::PROVISIONAL_MASK,
                    )
        });
        let reject_test = words
            .windows(2)
            .position(|window| window[0] == 0x6a11_021f && window[1] & 0xff00_001f == 0x5400_0001);
        assert!(
            matches!((reject_mask, reject_test), (Some(mask), Some(test)) if mask < test),
            "own dictionary proof rejects current opaque/provisional state"
        );
        assert!(
            words.iter().any(|word| word & 0xfff8_001f == 0x3710_000e),
            "own dictionary write rejects current prototype role: TBNZ w14, #2"
        );
    }
    #[cfg(target_arch = "x86_64")]
    {
        use yaxpeax_x86::amd64::{Opcode, Operand};
        let instructions = native_code::decode(code, start, end);
        for (mask, branch, fact) in [
            (
                otter_vm::object::ShapeState::DICTIONARY_MASK,
                Opcode::JZ,
                "dictionary kind",
            ),
            (
                otter_vm::object::ShapeState::OPAQUE_LOOKUP_MASK
                    | otter_vm::object::ShapeState::PROVISIONAL_MASK,
                Opcode::JNZ,
                "ordinary finalized lookup",
            ),
            (
                otter_vm::object::ShapeState::PROTOTYPE_MASK,
                Opcode::JNZ,
                "unwatched store receiver",
            ),
        ] {
            assert!(instructions.windows(2).any(|window| {
                window[0].opcode() == Opcode::TEST
                    && matches!(window[0].operand(1), Operand::ImmediateI8 { imm } if imm == mask as i8)
                    && window[1].opcode() == branch
            }), "own dictionary ShapeState guard: {fact}");
        }
    }
}

/// Eligible immutable identity is the ordinary global guard's complete layout
/// contract. Its producer rejects prototype, opaque and provisional shapes;
/// freeze or a role/descriptor change installs a different shape before use.
fn assert_ordinary_store_guard(code: &[u8], start: usize, end: usize) {
    #[cfg(target_arch = "aarch64")]
    {
        let words = code[start..end]
            .chunks_exact(4)
            .map(|bytes| u32::from_le_bytes(bytes.try_into().unwrap()))
            .collect::<Vec<_>>();
        let shape_load = words
            .iter()
            .position(|word| {
                word & 0xffc0_001f == 0xb940_0010 // LDR w16, [header, #shape]
            })
            .expect("own ordinary header shape read");
        let identity = words
            .windows(2)
            .position(|window| {
                window[0] == 0x6b11_021f // CMP w16, w17 (expected compressed shape)
                && window[1] & 0xff00_001f == 0x5400_0001 // B.NE miss
            })
            .expect("own exact shape comparison branches before the hit");
        assert!(shape_load < identity);
        assert!(
            !words.contains(&DICTIONARY_STATE_TEST),
            "the own operation uses exact ordinary identity, not dictionary layout"
        );
    }
    #[cfg(target_arch = "x86_64")]
    {
        use yaxpeax_x86::amd64::{Opcode, Operand, RegSpec};
        let instructions = native_code::decode(code, start, end);
        let shape_load = instructions.iter().position(|instruction| {
            instruction.opcode() == Opcode::MOV
                && matches!(instruction.operand(0), Operand::Register { reg } if reg == RegSpec::r9d())
                && native_code::base_offset(instruction.operand(1))
                    .is_some_and(|(base, _)| base == RegSpec::r8())
        }).expect("own ordinary header shape read");
        let identity = instructions.windows(2).position(|window| {
            window[0].opcode() == Opcode::CMP
                && matches!(window[0].operand(0), Operand::Register { reg } if reg == RegSpec::r9d())
                && matches!(window[0].operand(1), Operand::Register { reg } if reg == RegSpec::r10d())
                && window[1].opcode() == Opcode::JNZ
        }).expect("own exact shape comparison branches before the hit");
        assert!(shape_load < identity);
        assert!(
            !instructions.iter().any(|instruction| {
                instruction.opcode() == Opcode::TEST
                    && matches!(instruction.operand(1), Operand::ImmediateI8 { imm }
                        if imm == otter_vm::object::ShapeState::DICTIONARY_MASK as i8)
            }),
            "the own operation uses exact ordinary identity, not dictionary layout"
        );
    }
}

fn assert_dictionary_store_emission(bundle: &JitArtifactBundle, strict: bool) {
    assert_guarded_global_store_emission(bundle, strict, true);
}

fn assert_guarded_global_store_emission(
    bundle: &JitArtifactBundle,
    strict: bool,
    dictionary: bool,
) {
    let manifest = bundle.manifest();
    assert_eq!(manifest.architecture(), std::env::consts::ARCH);
    assert_eq!(manifest.operating_system(), std::env::consts::OS);
    assert_eq!(manifest.module(), MODULE);
    assert_eq!(manifest.entry(), JitDebugTarget::Entry);
    let code = bundle.file(JitArtifactFileName::Code).unwrap().contents();
    assert_eq!(code.len() as u64, manifest.code_bytes());
    let bytecode = binding_bytecode(bundle);
    let opcode = if strict {
        "StoreGlobalChecked"
    } else {
        "StoreGlobalBinding"
    };
    let stores = bytecode
        .iter()
        .filter(|(_, value)| value.as_str() == opcode)
        .collect::<Vec<_>>();
    assert_eq!(stores.len(), 1, "one own {opcode}");
    let byte_pc = *stores[0].0;
    let map = json(bundle, JitArtifactFileName::CodeMap);
    let regions = map["regions"].as_array().unwrap();
    let relocations = json(bundle, JitArtifactFileName::Relocations);
    let relocations = relocations["relocations"].as_array().unwrap();
    if manifest.tier() == JitDebugTier::Template {
        assert_binding_regions(bundle, true, false);
        if strict {
            assert_strict_global_store_operations(bundle);
        }
        let guards = regions
            .iter()
            .filter(|region| {
                region["kind"] == "templateBindingGuard"
                    && region["bytePc"].as_u64() == Some(u64::from(byte_pc))
            })
            .collect::<Vec<_>>();
        assert_eq!(guards.len(), 1, "own guarded global write");
        let assert_guard = if dictionary {
            assert_dictionary_store_guard
        } else {
            assert_ordinary_store_guard
        };
        assert_guard(
            code,
            guards[0]["startOffset"].as_u64().unwrap() as usize,
            guards[0]["endOffset"].as_u64().unwrap() as usize,
        );
        return;
    }
    assert_eq!(manifest.tier(), JitDebugTier::Optimizing);
    // Graph emits the sole Template binding owner inside its Generic node;
    // it deliberately has no Template subregion capture or entry counter.
    let own = regions
        .iter()
        .filter(|region| {
            region["kind"] == "instruction"
                && region["functionId"].as_u64() == Some(u64::from(manifest.function_id()))
                && region["bytePc"].as_u64() == Some(u64::from(byte_pc))
                && region["operation"]
                    .as_str()
                    .is_some_and(|operation| operation.contains(" Generic {"))
        })
        .collect::<Vec<_>>();
    assert_eq!(own.len(), 1, "one own Graph {opcode} operation");
    let start = own[0]["startOffset"].as_u64().unwrap() as usize;
    let end = own[0]["endOffset"].as_u64().unwrap() as usize;
    assert!(start < end && end <= code.len());
    if dictionary {
        assert_dictionary_store_guard(code, start, end);
    } else {
        assert_ordinary_store_guard(code, start, end);
    }
    let stubs = relocations
        .iter()
        .filter(|relocation| {
            relocation["target"]["kind"] == "runtimeStub"
                && relocation["startOffset"].as_u64().unwrap() >= start as u64
                && relocation["endOffset"].as_u64().unwrap() <= end as u64
        })
        .collect::<Vec<_>>();
    assert_eq!(
        calls(code, start, end),
        2,
        "one hit barrier and one committed cold call"
    );
    assert_eq!(stubs.len(), 2);
    for name in ["write_barrier", "jit_binding_value"] {
        assert_eq!(
            stubs
                .iter()
                .filter(|stub| stub["target"]["name"] == name)
                .count(),
            1,
            "own {opcode} contains the exact {name} descriptor"
        );
    }
    assert!(
        !stubs
            .iter()
            .any(|stub| stub["target"]["name"] == "jit_call_generic")
    );
    assert!(
        !relocations.iter().any(|relocation| {
            relocation["target"]["kind"] == "globalLexicalCell"
                && relocation["target"]["bytePc"].as_u64() == Some(u64::from(byte_pc))
        }),
        "the guarded write owns the global object, not a lexical cell"
    );
}

#[derive(Default)]
struct TraceLog {
    recording: bool,
    functions: Vec<String>,
}

struct Trace(Arc<Mutex<TraceLog>>);
impl StepTracer for Trace {
    fn on_step(&mut self, event: &StepEvent<'_>) {
        let mut log = self.0.lock().unwrap();
        if log.recording {
            log.functions.push(event.function_name.to_owned());
        }
    }
}

struct Proof {
    bundle: JitArtifactBundle,
    generation: JitCodeGenerationSnapshot,
}

struct BindingObservation {
    frames_json: String,
    generations: Vec<JitCodeGenerationSnapshot>,
    freeze: Option<Result<(), String>>,
}

#[derive(Default)]
struct BindingObserver {
    recording: AtomicBool,
    freeze: AtomicBool,
    records: Mutex<Vec<Result<BindingObservation, String>>>,
}

fn integrity_installer(observer: Arc<BindingObserver>) -> RuntimeExtensionInstaller {
    RuntimeExtensionInstaller::new(move |realm| {
        let observer = observer.clone();
        realm.install_native_global_call(
            "integrityObserve",
            0,
            RuntimeNativeCall::Dynamic(Arc::new(
                move |ctx: &mut RuntimeNativeCtx<'_>,
                      _args: &[RuntimeValue],
                      _state: &[RuntimeValue]| {
                    if observer.recording.load(Ordering::Relaxed) {
                        let frames = ctx
                            .execution_context()
                            .map(|context| ctx.capture_call_sites_json(context, 0, usize::MAX));
                        // This mutation goes through the rooted production
                        // native API. It preserves the realm lexical epoch and
                        // replaces the ordinary shape before publication; VM
                        // producer tests prove that geometry without a raw API.
                        let freeze = observer.freeze.load(Ordering::Relaxed).then(|| {
                            ctx.scope(|mut scope| {
                                let global = scope.global_this();
                                scope.freeze(global)
                            })
                            .map_err(|error| error.to_string())
                        });
                        let observed = frames
                            .map(|frames_json| BindingObservation {
                                frames_json,
                                generations: ctx.interp_mut().jit_code_generation_snapshot(),
                                freeze,
                            })
                            .ok_or_else(|| {
                                "binding observer has no active execution context".to_owned()
                            });
                        observer
                            .records
                            .lock()
                            .map_err(|_| RuntimeNativeError::Error {
                                message: "binding observation lock poisoned".into(),
                            })?
                            .push(observed);
                    }
                    Ok(RuntimeValue::undefined())
                },
            )),
        )
    })
}

struct Harness {
    runtime: Runtime,
    oracle: Runtime,
    trace: Arc<Mutex<TraceLog>>,
    proofs: BTreeMap<&'static str, Proof>,
    observer: Option<Arc<BindingObserver>>,
}

impl Harness {
    fn new(
        setup: &str,
        names: &[&'static str],
        abrupt: bool,
        installer: Option<RuntimeExtensionInstaller>,
    ) -> Self {
        Self::at_selection(
            setup,
            names,
            abrupt,
            installer,
            JitSelection::Template,
            None,
        )
    }

    fn at_selection(
        setup: &str,
        names: &[&'static str],
        abrupt: bool,
        installer: Option<RuntimeExtensionInstaller>,
        selection: JitSelection,
        observer: Option<Arc<BindingObserver>>,
    ) -> Self {
        let (debug_tier, frame_kind) = match selection {
            JitSelection::Template => (JitDebugTier::Template, NativeFrameKind::Baseline),
            JitSelection::ProductionTiered => {
                (JitDebugTier::Optimizing, NativeFrameKind::Optimizing)
            }
            JitSelection::InterpreterOnly => panic!("native harness requires a native selection"),
        };
        let trace = Arc::new(Mutex::new(TraceLog::default()));
        let mut builder = Runtime::builder()
            .jit_selection(selection)
            .jit_debug(JitDebugRequest::artifacts().with_events(true));
        if let Some(installer) = installer.clone() {
            builder = builder.extension_installer(installer);
        }
        let mut runtime = builder.build().unwrap();
        runtime.set_tracer(Some(Box::new(Trace(trace.clone()))));
        let mut oracle_builder = Runtime::builder().jit_selection(JitSelection::InterpreterOnly);
        if let Some(installer) = installer {
            oracle_builder = oracle_builder.extension_installer(installer);
        }
        let mut oracle = oracle_builder.build().unwrap();
        let result = runtime.run_script(SourceInput::from_javascript(setup), MODULE);
        let reference = oracle.run_script(SourceInput::from_javascript(setup), MODULE);
        let artifacts = if abrupt {
            assert!(result.is_err() && reference.is_err());
            runtime.take_jit_artifacts().unwrap()
        } else {
            let result = result.unwrap();
            assert_eq!(
                result.completion_string(),
                reference.unwrap().completion_string()
            );
            result.jit_artifacts().unwrap().clone()
        };
        let generations = runtime.jit_code_generation_snapshot();
        assert!(
            !artifacts.truncated(),
            "exact generation artifacts must be complete"
        );
        let proofs = names
            .iter()
            .map(|&name| {
                let proof = artifacts
                    .bundles()
                    .iter()
                    .filter(|bundle| {
                        bundle.manifest().module() == MODULE
                            && bundle.manifest().function_name() == name
                            && bundle.manifest().tier() == debug_tier
                            && bundle.manifest().entry() == JitDebugTarget::Entry
                    })
                    .find_map(|bundle| {
                        generations
                            .iter()
                            .find(|generation| {
                                generation.code_object_id == bundle.manifest().code_object_id()
                                    && generation.function_id == bundle.manifest().function_id()
                                    && generation.tier == frame_kind
                                    && generation.lifecycle == CodeLifetimeState::Installed
                                    && generation.linked
                            })
                            .map(|generation| Proof {
                                bundle: bundle.clone(),
                                generation: generation.clone(),
                            })
                    })
                    .unwrap_or_else(|| {
                        panic!("missing exact installed {name} generation: {generations:?}")
                    });
                (name, proof)
            })
            .collect();
        Self {
            runtime,
            oracle,
            trace,
            proofs,
            observer,
        }
    }

    fn probe(&mut self, source: &str, expected: &str, names: &[&'static str]) -> u64 {
        {
            let mut trace = self.trace.lock().unwrap();
            trace.functions.clear();
            trace.recording = true;
        }
        let before = self.runtime.execution_stats();
        if let Some(observer) = &self.observer {
            observer.records.lock().unwrap().clear();
            observer.recording.store(true, Ordering::Relaxed);
        }
        let result = self
            .runtime
            .run_script(
                SourceInput::from_javascript(source),
                "native-binding-probe.js",
            )
            .unwrap();
        let reference = self
            .oracle
            .run_script(
                SourceInput::from_javascript(source),
                "native-binding-probe.js",
            )
            .unwrap();
        if let Some(observer) = &self.observer {
            observer.recording.store(false, Ordering::Relaxed);
        }
        assert_eq!(result.completion_string(), expected);
        assert_eq!(reference.completion_string(), expected);
        let trace = self.trace.lock().unwrap();
        assert!(
            !trace.functions.is_empty(),
            "the independent interpreter tracer must observe the probe main"
        );
        for name in names {
            assert!(
                !trace.functions.iter().any(|function| function == name),
                "{name} must execute natively: {:?}",
                trace.functions
            );
        }
        drop(trace);
        let after = self.runtime.execution_stats();
        assert_eq!(
            after.jit_code_generations, before.jit_code_generations,
            "measured calls cannot compile replacement code"
        );
        let report = result.jit_debug_report().unwrap();
        assert!(!report.truncated());
        assert_eq!(report.dropped_events(), 0);
        assert!(
            !report.events().iter().any(|event| matches!(
                event,
                JitDebugEvent::Bail { .. }
                    | JitDebugEvent::EnteredGenerationDeopt { .. }
                    | JitDebugEvent::InlineDeoptFrame { .. }
            )),
            "committed binding misses cannot deopt or replay: {:?}",
            report.events()
        );
        let generations = self.runtime.jit_code_generation_snapshot();
        for name in names.iter().copied().collect::<BTreeSet<_>>() {
            let proof = self.proofs.get_mut(name).unwrap();
            let generation = generations
                .iter()
                .find(|generation| generation.code_object_id == proof.generation.code_object_id)
                .unwrap();
            assert!(generation.linked && generation.lifecycle == CodeLifetimeState::Installed);
            if proof.generation.tier == NativeFrameKind::Baseline {
                assert_eq!(
                    generation.generated_entries,
                    proof.generation.generated_entries
                        + names.iter().filter(|&&candidate| candidate == name).count() as u64
                );
            }
            assert_eq!(
                generation.generated_deopts,
                proof.generation.generated_deopts
            );
            assert_eq!(generation.active_count, 0);
            proof.generation = generation.clone();
        }
        if let Some(observer) = &self.observer {
            self.assert_observed_generations(observer, names);
        }
        after.jit_reentrant_stub_transitions - before.jit_reentrant_stub_transitions
    }

    fn assert_observed_generations(&self, observer: &BindingObserver, names: &[&'static str]) {
        let records = observer.records.lock().unwrap();
        assert_eq!(
            records.len(),
            names.len() * 2,
            "native and interpreter each observe every own invocation"
        );
        for (index, record) in records.iter().enumerate() {
            let record = record
                .as_ref()
                .unwrap_or_else(|error| panic!("panic-free binding observation: {error}"));
            let name = names[index % names.len()];
            let frames: serde_json::Value = serde_json::from_str(&record.frames_json).unwrap();
            let frames = frames.as_array().unwrap();
            assert_eq!(frames.len(), 2, "own writer and cold probe main");
            assert_eq!(frames[0]["functionName"], name);
            assert!(frames[0]["scriptName"].as_str().unwrap().ends_with(MODULE));
            assert_eq!(frames[0]["sourceLine"], "  observe();");
            assert_eq!(frames[1]["functionName"], "<main>");
            assert_eq!(
                record.freeze.is_some(),
                observer.freeze.load(Ordering::Relaxed),
                "the exact requested native mutation ran inside this own caller"
            );
            if let Some(freeze) = &record.freeze {
                assert!(freeze.is_ok(), "scoped heap-only freeze failed: {freeze:?}");
            }
            if index < names.len() {
                let proof = &self.proofs[name];
                let current = record
                    .generations
                    .iter()
                    .filter(|generation| {
                        generation.function_id == proof.generation.function_id
                            && generation.tier == proof.generation.tier
                            && generation.lifecycle == CodeLifetimeState::Installed
                            && generation.linked
                    })
                    .collect::<Vec<_>>();
                assert_eq!(
                    current.len(),
                    1,
                    "one current own generation at the actual observer call"
                );
                assert_eq!(
                    current[0].code_object_id,
                    proof.bundle.manifest().code_object_id()
                );
                assert_eq!(
                    current[0].generated_deopts,
                    proof.generation.generated_deopts
                );
            } else {
                assert!(
                    record.generations.is_empty(),
                    "the semantic oracle stays interpreted"
                );
            }
        }
    }
}

const INTEGRITY_SETUP: &str = r#"
globalThis.integrityBinding = 5;
for (let index = 0; index < 160; index++) globalThis['integrityPadding' + index] = index;
function strictIntegrityStore(value, observe) {
  'use strict';
  observe();
  integrityBinding = value;
  return value;
}
function sloppyIntegrityStore(value, observe) {
  observe();
  integrityBinding = value;
  return value;
}
for (let warm = 0; warm < 5000; warm++) {
  strictIntegrityStore(5, integrityObserve);
  sloppyIntegrityStore(5, integrityObserve);
}
"#;

const ORDINARY_INTEGRITY_SETUP: &str = r#"
globalThis.integrityBinding = 5;
// A normal named-property read migrates this small actual global. No delete,
// padding, prototype publication or descriptor override precedes admission.
globalThis.integrityBinding;
function strictIntegrityStore(value, observe) {
  'use strict';
  observe();
  integrityBinding = value;
  return value;
}
function sloppyIntegrityStore(value, observe) {
  observe();
  integrityBinding = value;
  return value;
}
for (let warm = 0; warm < 5000; warm++) {
  strictIntegrityStore(5, integrityObserve);
  sloppyIntegrityStore(5, integrityObserve);
}
"#;

fn integrity_harness(selection: JitSelection) -> Harness {
    integrity_harness_for(selection, INTEGRITY_SETUP, true)
}

fn integrity_harness_for(selection: JitSelection, setup: &str, dictionary: bool) -> Harness {
    let observer = Arc::new(BindingObserver::default());
    let mut harness = Harness::at_selection(
        setup,
        &["strictIntegrityStore", "sloppyIntegrityStore"],
        false,
        Some(integrity_installer(observer.clone())),
        selection,
        Some(observer),
    );
    for (name, strict) in [
        ("strictIntegrityStore", true),
        ("sloppyIntegrityStore", false),
    ] {
        if dictionary {
            assert_dictionary_store_emission(&harness.proofs[name].bundle, strict);
        } else {
            assert_guarded_global_store_emission(&harness.proofs[name].bundle, strict, false);
        }
        assert_integrity_observer_call(&harness.proofs[name].bundle);
    }
    assert_eq!(harness.probe(
        "JSON.stringify([strictIntegrityStore(7, integrityObserve), sloppyIntegrityStore(7, integrityObserve), integrityBinding]);",
        "[7,7,7]",
        &["strictIntegrityStore", "sloppyIntegrityStore"]), 0,
        "both exact installed global write hits are reached before integrity mutation");
    harness
}

fn assert_integrity_observer_call(bundle: &JitArtifactBundle) {
    let map = json(bundle, JitArtifactFileName::CodeMap);
    let regions = map["regions"].as_array().unwrap();
    let optimizing = bundle.manifest().tier() == JitDebugTier::Optimizing;
    let calls = regions
        .iter()
        .filter(|region| {
            region["kind"] == "instruction"
                && region["functionId"].as_u64() == Some(u64::from(bundle.manifest().function_id()))
                && region["operation"].as_str().is_some_and(|operation| {
                    if optimizing {
                        operation.contains(" CallJs {")
                    } else {
                        operation.starts_with("Call {")
                    }
                })
        })
        .collect::<Vec<_>>();
    assert_eq!(
        calls.len(),
        1,
        "the actual observer owns this function's sole JS call"
    );
    let call = calls[0];
    let byte_pc = u32::try_from(call["bytePc"].as_u64().unwrap()).unwrap();
    assert_eq!(binding_bytecode(bundle)[&byte_pc], "Call");
    let relocations = json(bundle, JitArtifactFileName::Relocations);
    let links = relocations["relocations"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|relocation| {
            relocation["target"]["kind"] == "runtimeStub"
                && relocation["target"]["id"].as_u64()
                    == Some(u64::from(otter_vm::native_abi::STUB_JIT_CALL_GENERIC.id))
                && relocation["target"]["name"] == "jit_call_generic"
                && relocation["target"]["signature"] == "jsCall"
                && relocation["startOffset"].as_u64().unwrap()
                    >= call["startOffset"].as_u64().unwrap()
                && relocation["endOffset"].as_u64().unwrap() <= call["endOffset"].as_u64().unwrap()
        })
        .collect::<Vec<_>>();
    assert_eq!(links.len(), 1, "own observer private-JS native edge");
    let end = links[0]["endOffset"].as_u64().unwrap() as usize;
    let code = bundle.file(JitArtifactFileName::Code).unwrap().contents();
    let call_end = call["endOffset"].as_u64().unwrap() as usize;
    #[cfg(target_arch = "aarch64")]
    assert!(end + 4 <= call_end && call_end <= code.len());
    #[cfg(target_arch = "x86_64")]
    assert!(end + 3 <= call_end && call_end <= code.len());
    #[cfg(target_arch = "aarch64")]
    assert_eq!(
        u32::from_le_bytes(code[end..end + 4].try_into().unwrap()),
        0xd63f_0200,
        "actual BLR x16"
    );
    #[cfg(target_arch = "x86_64")]
    assert_eq!(&code[end..end + 3], &[0x41, 0xff, 0xd3], "actual CALL r11");
}

#[test]
fn heap_only_frozen_ordinary_globals_decline_current_native_strict_and_sloppy_stores() {
    for selection in [JitSelection::Template, JitSelection::ProductionTiered] {
        let mut harness = integrity_harness_for(selection, ORDINARY_INTEGRITY_SETUP, false);
        let observer = harness.observer.as_ref().unwrap().clone();
        observer.freeze.store(true, Ordering::Relaxed);
        // Each runtime's actual observer performs heap-only scoped freeze
        // immediately before this same admitted caller reaches the store.
        // Its lexical epoch stays equal and the old immutable shape is retired;
        // the exact installed ordinary proof must miss before writing.
        assert_eq!(harness.probe(
            "{ let effects = 0; let kind = ''; try { strictIntegrityStore(++effects, integrityObserve); } catch (error) { kind = error.name; } const descriptor = Object.getOwnPropertyDescriptor(globalThis, 'integrityBinding'); JSON.stringify([kind, effects, integrityBinding, descriptor.writable, descriptor.configurable, Object.isFrozen(globalThis)]); }",
            "[\"TypeError\",1,7,false,false,true]", &["strictIntegrityStore"]), 2,
            "heap-only freeze refuses both own ordinary strict guards before mutation, without replacing their generation");
        observer.freeze.store(false, Ordering::Relaxed);
        assert_eq!(harness.probe(
            "{ let effects = 0; const result = sloppyIntegrityStore(++effects, integrityObserve); const descriptor = Object.getOwnPropertyDescriptor(globalThis, 'integrityBinding'); JSON.stringify([result, effects, integrityBinding, descriptor.writable, descriptor.configurable]); }",
            "[1,1,7,false,false]", &["sloppyIntegrityStore"]), 1,
            "the same ordinary sloppy generation commits silent rejection exactly once");
    }
}

#[test]
fn frozen_dictionary_globals_decline_current_native_strict_and_sloppy_stores() {
    for selection in [JitSelection::Template, JitSelection::ProductionTiered] {
        let mut harness = integrity_harness(selection);
        // The observer is an actual parameter: freezing the global cannot
        // turn an observer-name load into an unrelated binding cold call.
        assert_eq!(harness.probe(
            "{ let effects = 0; let kind = ''; Object.freeze(globalThis); try { strictIntegrityStore(++effects, integrityObserve); } catch (error) { kind = error.name; } const descriptor = Object.getOwnPropertyDescriptor(globalThis, 'integrityBinding'); JSON.stringify([kind, effects, integrityBinding, descriptor.writable, descriptor.configurable, Object.isFrozen(globalThis)]); }",
            "[\"TypeError\",1,7,false,false,true]", &["strictIntegrityStore"]), 2,
            "freeze retires both strict existence and checked-store guards without replacing their generation");
        assert_eq!(harness.probe(
            "{ let effects = 0; const result = sloppyIntegrityStore(++effects, integrityObserve); const descriptor = Object.getOwnPropertyDescriptor(globalThis, 'integrityBinding'); JSON.stringify([result, effects, integrityBinding, descriptor.writable, descriptor.configurable]); }",
            "[1,1,7,false,false]", &["sloppyIntegrityStore"]), 1,
            "the frozen sloppy store commits silent rejection once through its current generation");
    }
}

#[test]
fn sealed_dictionary_globals_retire_descriptor_guards_and_keep_writable_stores() {
    for selection in [JitSelection::Template, JitSelection::ProductionTiered] {
        let mut harness = integrity_harness(selection);
        assert_eq!(harness.probe(
            "{ let effects = 0; Object.seal(globalThis); const result = strictIntegrityStore(++effects + 10, integrityObserve); const descriptor = Object.getOwnPropertyDescriptor(globalThis, 'integrityBinding'); JSON.stringify([result, effects, integrityBinding, descriptor.writable, descriptor.configurable, Object.isSealed(globalThis), Object.isFrozen(globalThis)]); }",
            "[11,1,11,true,false,true,false]", &["strictIntegrityStore"]), 2,
            "seal retires the watched descriptor guards while the strict store remains writable");
        assert_eq!(harness.probe(
            "{ let effects = 0; const result = sloppyIntegrityStore(++effects + 20, integrityObserve); const descriptor = Object.getOwnPropertyDescriptor(globalThis, 'integrityBinding'); JSON.stringify([result, effects, integrityBinding, descriptor.writable, descriptor.configurable]); }",
            "[21,1,21,true,false]", &["sloppyIntegrityStore"]), 1,
            "the sealed sloppy store commits one writable store through the same native generation");
    }
}

#[test]
fn global_access_completes_from_its_own_loop_osr() {
    let source = r#"
var acc = 0;
var setterCalls = 0;
globalThis._sensor = 0;
Object.defineProperty(globalThis, "sensor", {
  get() { return this._sensor; },
  set(v) { setterCalls++; this._sensor = v; }, configurable: true,
});
function globalAccessorLoop(rounds) {
  for (let round = 0; round < rounds; round++) { acc = acc + round; sensor = acc; }
  return [acc, sensor, setterCalls].join(":");
}
globalAccessorLoop(4096);
"#;
    let mut oracle = Runtime::builder()
        .jit_selection(JitSelection::InterpreterOnly)
        .build()
        .unwrap();
    let reference = oracle
        .run_script(SourceInput::from_javascript(source), MODULE)
        .unwrap();
    let mut runtime = Runtime::builder()
        .jit_selection(JitSelection::Template)
        .jit_debug(JitDebugRequest::artifacts())
        .build()
        .unwrap();
    let result = runtime
        .run_script(SourceInput::from_javascript(source), MODULE)
        .unwrap();
    assert_eq!(reference.completion_string(), "8386560:8386560:4096");
    assert_eq!(result.completion_string(), reference.completion_string());
    let bundle = result
        .jit_artifacts()
        .unwrap()
        .bundles()
        .iter()
        .find(|bundle| {
            bundle.manifest().module() == MODULE
                && bundle.manifest().function_name() == "globalAccessorLoop"
                && bundle.manifest().tier() == JitDebugTier::Template
                && matches!(bundle.manifest().entry(), JitDebugTarget::Osr { .. })
        })
        .expect("this exact loop owns the Template OSR artifact");
    assert_binding_regions(bundle, true, false);
    assert!(runtime.execution_stats().jit_osr_attempts > 0);
    assert!(runtime.execution_stats().jit_reentrant_stub_transitions > 0);
}

#[test]
fn lexical_cells_read_live_values_and_const_and_tdz_misses_commit_once() {
    let setup = r#"
let liveBinding = 3;
const fixedBinding = 11;
function readLiveBinding() { return liveBinding; }
function writeLiveBinding(value) { liveBinding = value; return value; }
function writeFixedBinding(value) { fixedBinding = value; }
for (let warm = 0; warm < 5000; warm++) {
  readLiveBinding(); writeLiveBinding(3);
  try { writeFixedBinding(12); } catch (error) {}
}
"#;
    let mut harness = Harness::new(
        setup,
        &["readLiveBinding", "writeLiveBinding", "writeFixedBinding"],
        false,
        None,
    );
    assert_binding_regions(&harness.proofs["readLiveBinding"].bundle, true, true);
    assert_binding_regions(&harness.proofs["writeLiveBinding"].bundle, true, true);
    assert_binding_regions(&harness.proofs["writeFixedBinding"].bundle, false, false);
    assert_eq!(
        harness.probe(
            "JSON.stringify([readLiveBinding(), writeLiveBinding(17), readLiveBinding()]);",
            "[3,17,17]",
            &["readLiveBinding", "writeLiveBinding", "readLiveBinding"]
        ),
        0
    );
    assert_eq!(harness.probe("{ let effects = 0; let kind = ''; try { writeFixedBinding(++effects); } catch (error) { kind = error.name; } JSON.stringify([kind, effects, fixedBinding]); }", "[\"TypeError\",1,11]", &["writeFixedBinding"]), 1);

    let setup = r#"
function readHoleBinding(trigger) { if (trigger) return holeBinding; return 7; }
for (let warm = 0; warm < 5000; warm++) readHoleBinding(false);
throw new Error('leave the already-declared lexical cell uninitialized');
let holeBinding = 17;
"#;
    let mut harness = Harness::new(setup, &["readHoleBinding"], true, None);
    assert_binding_regions(&harness.proofs["readHoleBinding"].bundle, true, true);
    assert_eq!(harness.probe("{ let effects = 0; let kind = ''; try { readHoleBinding(++effects); } catch (error) { kind = error.name; } JSON.stringify([kind, effects]); }", "[\"ReferenceError\",1]", &["readHoleBinding"]), 1);
}

#[test]
fn global_epoch_dictionary_and_descriptor_misses_preserve_current_native_body() {
    let setup = r#"
globalThis.epochBinding = 3;
globalThis.dictionaryBinding = 5;
let descriptorGetterCalls = 0;
let descriptorSetterCalls = 0;
let descriptorStored = 0;
for (let index = 0; index < 160; index++) globalThis['bindingPadding' + index] = index;
function readEpochBinding() { return epochBinding; }
function readDictionaryBinding() { return dictionaryBinding; }
function writeDictionaryBinding(value) { 'use strict'; dictionaryBinding = value; return value; }
for (let warm = 0; warm < 5000; warm++) {
  readEpochBinding(); readDictionaryBinding(); writeDictionaryBinding(5);
}
"#;
    let names = [
        "readEpochBinding",
        "readDictionaryBinding",
        "writeDictionaryBinding",
    ];
    // Each miss starts with the original epoch and descriptor layout, so an
    // earlier epoch failure cannot hide the dictionary or descriptor guard.
    let fresh = || {
        let harness = Harness::new(setup, &names, false, None);
        for proof in harness.proofs.values() {
            assert_binding_regions(&proof.bundle, true, false);
        }
        assert_strict_global_store_operations(&harness.proofs["writeDictionaryBinding"].bundle);
        harness
    };
    let mut harness = fresh();
    assert_eq!(harness.probe("JSON.stringify([readEpochBinding(), writeDictionaryBinding(9), readDictionaryBinding()]);", "[3,9,9]", &["readEpochBinding", "writeDictionaryBinding", "readDictionaryBinding"]), 0);
    assert_eq!(
        harness.probe(
            "globalThis.dictionaryBinding = 13; readDictionaryBinding();",
            "13",
            &["readDictionaryBinding"]
        ),
        0,
        "same-slot value replacement stays a live native hit"
    );
    let mut harness = fresh();
    assert_eq!(
        harness.probe(
            "let epochBinding = 29; readEpochBinding();",
            "29",
            &["readEpochBinding"]
        ),
        1
    );
    let mut harness = fresh();
    assert_eq!(
        harness.probe(
            "globalThis.bindingAddedAfterCompile = 1; readDictionaryBinding();",
            "5",
            &["readDictionaryBinding"]
        ),
        0,
        "appending an unrelated key preserves the watched slot layout"
    );
    let mut harness = fresh();
    assert_eq!(
        harness.probe(
            "delete globalThis.dictionaryBinding; globalThis.dictionaryBinding = 5; readDictionaryBinding();",
            "5",
            &["readDictionaryBinding"]
        ),
        1,
        "removing the watched slot changes its guarded dictionary layout"
    );
    let mut harness = fresh();
    assert_eq!(harness.probe("Object.defineProperty(globalThis, 'dictionaryBinding', {get() { descriptorGetterCalls++; return 41; }, configurable: true}); JSON.stringify([readDictionaryBinding(), descriptorGetterCalls]);", "[41,1]", &["readDictionaryBinding"]), 1);
    let mut harness = fresh();
    // Strict assignment snapshots existence before the RHS, then performs
    // the checked store. The changed layout makes both exact operations cold;
    // argument evaluation and the throwing store still happen once.
    assert_eq!(harness.probe("{ let effects = 0; let kind = ''; Object.defineProperty(globalThis, 'dictionaryBinding', {value: 43, writable: false, configurable: true}); try { writeDictionaryBinding(++effects); } catch (error) { kind = error.name; } JSON.stringify([kind, effects, dictionaryBinding]); }", "[\"TypeError\",1,43]", &["writeDictionaryBinding"]), 2);
    let mut harness = fresh();
    assert_eq!(harness.probe("Object.defineProperty(globalThis, 'dictionaryBinding', {get() { return descriptorStored; }, set(value) { descriptorSetterCalls++; descriptorStored = value; }, configurable: true}); JSON.stringify([writeDictionaryBinding(47), descriptorSetterCalls, descriptorStored]);", "[47,1,47]", &["writeDictionaryBinding"]), 2);
}

#[test]
fn native_binding_writes_keep_exact_young_children_through_moving_collection() {
    let observations = Arc::new(Mutex::new(Vec::<
        Result<Vec<moving_children::ChildMotion>, String>,
    >::new()));
    let installer = {
        let observations = observations.clone();
        RuntimeExtensionInstaller::new(move |realm| {
            moving_children::install(realm)?;
            realm.install_native_global_call(
                "bindingStress",
                1,
                RuntimeNativeCall::Dynamic(Arc::new(
                    |ctx: &mut RuntimeNativeCtx<'_>,
                     args: &[RuntimeValue],
                     _state: &[RuntimeValue]| {
                        let stride = args
                            .first()
                            .and_then(|value| value.as_number())
                            .ok_or_else(|| RuntimeNativeError::Error {
                                message: "missing binding stress stride".into(),
                            })?
                            .as_f64() as u32;
                        ctx.interp_mut()
                            .gc_heap_mut()
                            .set_gc_stress(stride, stride != 0);
                        Ok(RuntimeValue::undefined())
                    },
                )),
            )?;
            let observations = observations.clone();
            realm.install_native_global_call(
                "bindingCollectChildren",
                1,
                RuntimeNativeCall::Dynamic(Arc::new(
                    move |ctx: &mut RuntimeNativeCtx<'_>,
                          args: &[RuntimeValue],
                          _state: &[RuntimeValue]| {
                        let motion = moving_children::observe_and_collect(ctx, args)
                            .map_err(|error| format!("{error:?}"));
                        observations.lock().unwrap().push(motion);
                        Ok(RuntimeValue::undefined())
                    },
                )),
            )
        })
    };
    let setup = r#"
let lexicalChild = {marker: 7};
globalThis.objectChild = lexicalChild;
function storeLexicalChild(value) { lexicalChild = value; return value; }
function storeObjectChild(value) { objectChild = value; return value; }
function readLexicalChild() { return lexicalChild; }
function readObjectChild() { return objectChild; }
for (let warm = 0; warm < 5000; warm++) {
  storeLexicalChild(lexicalChild); storeObjectChild(lexicalChild);
  readLexicalChild(); readObjectChild();
}
"#;
    let names = [
        "storeLexicalChild",
        "storeObjectChild",
        "readLexicalChild",
        "readObjectChild",
    ];
    let mut harness = Harness::new(setup, &names, false, Some(installer));
    for (name, proof) in &harness.proofs {
        assert_binding_regions(&proof.bundle, true, name.contains("Lexical"));
    }
    harness.runtime.force_gc().unwrap();
    harness.oracle.force_gc().unwrap();
    for stride in 1..=16 {
        observations.lock().unwrap().clear();
        let before = harness.runtime.execution_stats();
        let source = format!(
            "{{ bindingStress({stride}); const child = {{marker: 7}}; storeLexicalChild(child); storeObjectChild(child); bindingCollectChildren(child); bindingStress(0); JSON.stringify([readLexicalChild() === child, readObjectChild() === child, lexicalChild.marker, objectChild.marker]); }}"
        );
        assert_eq!(harness.probe(&source, "[true,true,7,7]", &names), 0);
        let after = harness.runtime.execution_stats();
        assert!(
            after.gc_minor_slot_updates > before.gc_minor_slot_updates,
            "real moving collection at stride {stride}"
        );
        let observations = observations.lock().unwrap();
        assert_eq!(
            observations.len(),
            2,
            "candidate and semantic oracle each collect their own child"
        );
        for observation in observations.iter() {
            let children = observation
                .as_ref()
                .expect("panic-free collection succeeded");
            assert_eq!(children.len(), 1);
            assert_ne!(
                children[0].before, children[0].after,
                "the exact child must move at stride {stride}"
            );
            assert_eq!(children[0].marker_before, 7.0);
            assert_eq!(children[0].marker_after, 7.0);
        }
    }
}
