//! Ordinary intrinsic allocation proofs across collection and late replacement.
//!
//! # Contents
//! - Default and additional source-realm empty Array/Object allocations.
//! - Held callable cells after full collection and exact late identity misses.
//! - Escaped Array prototype identity after its source realm is disposed.
//! - Implicit errors from retained factories called across source realms.
//!
//! # Invariants
//! - Execution and artifacts follow each runtime's declared tier policy;
//!   production admission uses normal execution without manual promotion.
//! - A cold forwarding branch retains the subject's own entry and argument frame.
//! - A miss preserves evaluated arguments and does not replay preceding effects.
//! - Returned objects use the source realm's canonical prototypes.
//! - Collecting error sites retain their exact source operation and constant pool.
//!
//! # See also
//! - The parent fixture verifies collecting allocation homes and actual OSR.
//! - `otter_vm::jit::intrinsic_calls` owns the callable proof.

use super::*;
use otter_runtime::{
    RuntimeExtensionInstaller, RuntimeNativeCall, RuntimeNativeCtx, RuntimeNativeError,
    RuntimeValue,
};
use otter_vm::PersistentRootId;
use std::sync::{Arc, Mutex};

const MODULE: &str = "native-intrinsic-call-proofs.js";
const SETUP: &str = r#"
const OriginalArray = Array, OriginalObject = Object;
let intrinsicEffects = 0;
function guardedEmptyAllocations() {
  const array = new OriginalArray();
  const object = OriginalObject();
  if (array.length < 0) return guardedEmptyAllocations.apply(null, arguments);
  return [array, object];
}
function guardedArrayLength(value) {
  intrinsicEffects++;
  return new Array(value);
}
for (let warm = 0; warm < 10000; warm++) {
  guardedEmptyAllocations(); guardedArrayLength(2);
}
intrinsicEffects = 0;
"#;

fn probe(extra_realm: bool) {
    let mut runtime = Runtime::builder()
        .jit_compile_threads(0)
        .jit_selection(JitSelection::ProductionTiered)
        .jit_debug(JitDebugRequest::artifacts().with_events(true))
        .build()
        .unwrap();
    let realm = extra_realm.then(|| runtime.create_realm().unwrap());
    let setup = run(&mut runtime, realm, SETUP, MODULE);
    let batch = setup
        .jit_artifacts()
        .expect("intrinsic compilation artifacts");
    let own: Vec<_> = batch
        .bundles()
        .iter()
        .filter(|bundle| {
            bundle.manifest().module() == MODULE
                && bundle.manifest().function_name() == "guardedEmptyAllocations"
                && bundle.manifest().tier() == JitDebugTier::Optimizing
        })
        .collect();
    assert!(
        !own.is_empty(),
        "normal policy must admit the intrinsic subject"
    );
    for bundle in own {
        let ir = std::str::from_utf8(
            bundle
                .file(JitArtifactFileName::OptimizedIr)
                .unwrap()
                .contents(),
        )
        .unwrap();
        assert!(ir.matches("CheckCellValue(").count() >= 2, "{ir}");
        assert!(
            ir.contains("NewArrayEmpty []") && ir.contains("NewObject []"),
            "{ir}"
        );
        let bytecode = std::str::from_utf8(
            bundle
                .file(JitArtifactFileName::Bytecode)
                .unwrap()
                .contents(),
        )
        .unwrap();
        assert!(
            bytecode.contains(" New ") && bytecode.contains(" Call "),
            "{bytecode}"
        );
    }
    drop(setup);
    runtime
        .force_gc()
        .expect("collect held intrinsic identity cells");
    let value = run(
        &mut runtime,
        realm,
        r#"
const pair = guardedEmptyAllocations();
const original = guardedArrayLength(3);
function Replacement(value) { this.value = value; this.tag = 7; }
globalThis.Array = Replacement;
const replaced = guardedArrayLength(pair[1]);
globalThis.Array = OriginalArray;
const recovered = guardedArrayLength(4);
JSON.stringify([pair[0].length, Object.getPrototypeOf(pair[0]) === OriginalArray.prototype,
 Object.getPrototypeOf(pair[1]) === OriginalObject.prototype, original.length,
 replaced.tag, replaced.value === pair[1], recovered.length, intrinsicEffects]);
"#,
        "native-intrinsic-call-proof-probe.js",
    );
    assert_eq!(value.completion_string(), "[0,true,true,3,7,true,4,3]");
    let report = value.jit_debug_report().unwrap();
    assert!(!report.truncated());
    assert!(
        report.events().iter().any(|event| matches!(
            event,
            JitDebugEvent::Bail {
                exit_reason: ExitReason::IdentityGuard,
                ..
            } | JitDebugEvent::EnteredGenerationDeopt {
                exit_reason: ExitReason::IdentityGuard,
                ..
            } | JitDebugEvent::InlineDeoptFrame { .. }
        )),
        "late replacement must reach the evaluated-callee guard: {report:?}"
    );
    drop(value);
    runtime
        .force_gc()
        .expect("release native roots after the identity miss");
    assert_eq!(
        run(
            &mut runtime,
            realm,
            "guardedArrayLength(5).length",
            "native-intrinsic-reuse.js"
        )
        .completion_string(),
        "5"
    );
}

#[test]
fn ordinary_intrinsics_keep_held_identity_and_default_source_realm() {
    probe(false);
}

#[test]
fn ordinary_intrinsics_keep_held_identity_and_additional_source_realm() {
    probe(true);
}

fn clone_intrinsic_value(
    ctx: &mut RuntimeNativeCtx<'_>,
    args: &[RuntimeValue],
) -> Result<RuntimeValue, RuntimeNativeError> {
    let value = args
        .first()
        .copied()
        .unwrap_or_else(RuntimeValue::undefined);
    otter_runtime::web_structured_clone::structured_clone(ctx, value)
}

fn exchange_installer(slot: Arc<Mutex<Option<PersistentRootId>>>) -> RuntimeExtensionInstaller {
    RuntimeExtensionInstaller::new(move |realm| {
        realm.install_native_global("cloneIntrinsicValue", 1, clone_intrinsic_value)?;
        let saved = slot.clone();
        realm.install_native_global_call(
            "saveIntrinsicBundle",
            1,
            RuntimeNativeCall::Dynamic(Arc::new(
                move |ctx: &mut RuntimeNativeCtx<'_>, args: &[RuntimeValue], _: &[RuntimeValue]| {
                    ctx.scope(|mut scope| {
                        let value = scope.argument(args, 0);
                        let mut slot = saved
                            .lock()
                            .map_err(|_| RuntimeNativeError::InvalidOperand)?;
                        if slot.is_some() {
                            return Err(RuntimeNativeError::InvalidOperand);
                        }
                        *slot = Some(scope.persistent_root_insert(value));
                        Ok(RuntimeValue::undefined())
                    })
                },
            )),
        )?;
        let loaded = slot.clone();
        realm.install_native_global_call(
            "loadIntrinsicBundle",
            0,
            RuntimeNativeCall::Dynamic(Arc::new(
                move |ctx: &mut RuntimeNativeCtx<'_>, _: &[RuntimeValue], _: &[RuntimeValue]| {
                    let id = loaded
                        .lock()
                        .map_err(|_| RuntimeNativeError::InvalidOperand)?
                        .take()
                        .ok_or(RuntimeNativeError::InvalidOperand)?;
                    ctx.scope(|mut scope| {
                        let value = scope
                            .take_persistent_root(id)
                            .ok_or(RuntimeNativeError::InvalidOperand)?;
                        Ok(scope.finish(value))
                    })
                },
            )),
        )
    })
}

#[test]
fn default_source_arrays_keep_their_prototype_in_a_foreign_caller() {
    for selection in [
        JitSelection::InterpreterOnly,
        JitSelection::Template,
        JitSelection::ProductionTiered,
    ] {
        let slot = Arc::new(Mutex::new(None));
        let mut runtime = Runtime::builder()
            .jit_compile_threads(0)
            .jit_selection(selection)
            .jit_debug(JitDebugRequest::artifacts().with_events(true))
            .extension_installer(exchange_installer(slot))
            .build()
            .unwrap();
        run(
            &mut runtime,
            None,
            r#"
function defaultArrayFactory(child) {
  const literal = [], called = Array(), constructed = new Array(), sized = new Array(1);
  literal.push(child); called.push(child); constructed.push(child); sized[0] = child;
  return [literal, called, constructed, sized];
}

const warmIntrinsicChild = {marker: 7};
for (let i = 0; i < 10000; i++) defaultArrayFactory(warmIntrinsicChild);
saveIntrinsicBundle([defaultArrayFactory, Array.prototype]);
"#,
            "default-intrinsic-source.js",
        );
        let realm = runtime.create_realm().unwrap();
        run(
            &mut runtime,
            Some(realm),
            "const intrinsicBundle = loadIntrinsicBundle(); const foreignChild = {marker: 7};",
            "foreign-intrinsic-setup.js",
        );
        runtime
            .force_gc()
            .expect("move the exchanged callable and canonical prototype");
        let result = run(
            &mut runtime,
            Some(realm),
            r#"
const foreignArrays = intrinsicBundle[0](foreignChild);
JSON.stringify(foreignArrays.map(value => [value[0] === foreignChild,
 Object.getPrototypeOf(value) === intrinsicBundle[1],
 Object.getPrototypeOf(value) !== Array.prototype]));
"#,
            "foreign-intrinsic-probe.js",
        );
        assert_eq!(
            result.completion_string(),
            "[[true,true,true],[true,true,true],[true,true,true],[true,true,true]]",
            "{selection:?}"
        );
        drop(result);
        runtime
            .force_gc()
            .expect("retain the returned source arrays");
        assert_eq!(
            run(
                &mut runtime,
                Some(realm),
                "JSON.stringify(foreignArrays.map(value => value[0].marker))",
                "foreign-intrinsic-retained.js"
            )
            .completion_string(),
            "[7,7,7,7]"
        );
    }
}

#[test]
fn escaped_array_prototypes_keep_their_brand_after_realm_disposal() {
    for selection in [
        JitSelection::InterpreterOnly,
        JitSelection::Template,
        JitSelection::ProductionTiered,
    ] {
        let slot = Arc::new(Mutex::new(None));
        let mut runtime = Runtime::builder()
            .jit_compile_threads(0)
            .jit_selection(selection)
            .extension_installer(exchange_installer(slot))
            .build()
            .unwrap();
        let realm = runtime.create_realm().unwrap();
        run(
            &mut runtime,
            Some(realm),
            "saveIntrinsicBundle([Array.prototype, new Proxy(Array.prototype, {})]);",
            "escaped-array-prototypes-source.js",
        );
        run(
            &mut runtime,
            None,
            r#"
const escapedArrayPrototypes = (() => {
  const bundle = loadIntrinsicBundle();
  return [bundle[0], bundle[1]];
})();
"#,
            "escaped-array-prototypes-load.js",
        );
        runtime
            .force_gc()
            .expect("move the rooted prototype and proxy before source realm disposal");
        runtime.dispose_realm(realm).unwrap();
        runtime
            .force_gc()
            .expect("retain escaped prototypes without their realm roots");
        let result = run(
            &mut runtime,
            None,
            r#"
const escapedClone = cloneIntrinsicValue(escapedArrayPrototypes[0]);
JSON.stringify([escapedArrayPrototypes.map(value => [Array.isArray(value),
 Object.prototype.toString.call(value), JSON.stringify(value), [].concat(value).length]),
 [Array.isArray(escapedClone), Object.getPrototypeOf(escapedClone) === Array.prototype,
  escapedClone.length]]);
"#,
            "escaped-array-prototypes-probe.js",
        );
        assert_eq!(
            result.completion_string(),
            "[[[true,\"[object Array]\",\"[]\",0],[true,\"[object Array]\",\"[]\",0]],[true,true,0]]",
            "{selection:?}"
        );
    }
}

#[test]
fn array_prototype_length_rollback_and_clone_preserve_exotic_semantics() {
    for selection in [
        JitSelection::InterpreterOnly,
        JitSelection::Template,
        JitSelection::ProductionTiered,
    ] {
        let slot = Arc::new(Mutex::new(None));
        let mut runtime = Runtime::builder()
            .jit_compile_threads(0)
            .jit_selection(selection)
            .extension_installer(exchange_installer(slot))
            .build()
            .unwrap();
        let realm = runtime.create_realm().unwrap();
        let result = run(
            &mut runtime,
            Some(realm),
            r#"
const prototype = Array.prototype;
Object.defineProperty(prototype, "2", {value: {marker: 7}, writable: true, enumerable: true});
prototype[4] = {marker: 9};
const grown = prototype.length;
const truncated = Reflect.defineProperty(prototype, "length", {value: 1, writable: false});
const added = Reflect.set(prototype, "3", 11);
const clone = cloneIntrinsicValue(prototype);
saveIntrinsicBundle(prototype);
JSON.stringify([grown, truncated, prototype.length, Object.hasOwn(prototype, "4"),
 Object.getOwnPropertyDescriptor(prototype, "length").writable, added,
 Array.isArray(clone), clone.length, clone[2].marker,
 Object.hasOwn(clone, "0"), Object.getOwnPropertyDescriptor(clone, "length").writable]);
"#,
            "array-prototype-length-rollback.js",
        );
        assert_eq!(
            result.completion_string(),
            "[5,false,3,false,false,false,true,3,7,false,true]",
            "{selection:?}"
        );
        drop(result);
        runtime.dispose_realm(realm).unwrap();
        runtime.force_gc().unwrap();
        let retained = run(
            &mut runtime,
            None,
            r#"
const retainedPrototype = loadIntrinsicBundle();
const retainedClone = cloneIntrinsicValue(retainedPrototype);
JSON.stringify([Array.isArray(retainedPrototype), retainedPrototype.length,
 Array.isArray(retainedClone), retainedClone.length, retainedClone[2].marker,
 Object.hasOwn(retainedClone, "0")]);
"#,
            "array-prototype-retained-length.js",
        );
        assert_eq!(
            retained.completion_string(),
            "[true,3,true,3,7,false]",
            "{selection:?}"
        );
    }
}

fn assert_implicit_error_source(bundle: &JitArtifactBundle) {
    let bytecode = std::str::from_utf8(
        bundle
            .file(JitArtifactFileName::Bytecode)
            .expect("implicit-error bytecode")
            .contents(),
    )
    .unwrap();
    let sites: Vec<_> = bytecode
        .lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            let logical_pc = words.next()?.parse::<u64>().ok()?;
            let byte_pc = words.next()?.strip_prefix("byte=")?.parse::<u64>().ok()?;
            (words.next()? == "NewError").then_some((logical_pc, byte_pc))
        })
        .collect();
    assert_eq!(
        sites.len(),
        1,
        "one implicit destructuring error: {bytecode}"
    );
    let (logical_pc, byte_pc) = sites[0];
    let map = json(bundle, JitArtifactFileName::CodeMap);
    assert!(
        map["callEntryOffset"].as_u64().is_some(),
        "the retained factory must have its own JavaScript entry"
    );
    let regions: Vec<_> = map["regions"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|region| {
            region["kind"] == "instruction"
                && region["functionId"].as_u64() == Some(u64::from(bundle.manifest().function_id()))
                && region["logicalPc"].as_u64() == Some(logical_pc)
                && region["bytePc"].as_u64() == Some(byte_pc)
                && region["startOffset"].as_u64() < region["endOffset"].as_u64()
        })
        .collect();
    let relocations = json(bundle, JitArtifactFileName::Relocations);
    let scalars: Vec<_> = relocations["relocations"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|relocation| {
            relocation["target"]["kind"] == "runtimeStub"
                && relocation["target"]["id"] == otter_vm::native_abi::STUB_JIT_SCALAR_VALUE.id
                && relocation["startOffset"].as_u64().is_some_and(|offset| {
                    regions.iter().any(|region| {
                        region["startOffset"].as_u64().unwrap() <= offset
                            && offset < region["endOffset"].as_u64().unwrap()
                    })
                })
        })
        .collect();
    assert_eq!(
        scalars.len(),
        1,
        "the source error enters its canonical collecting scalar helper once"
    );
    let helper = scalars[0]["startOffset"].as_u64().unwrap() as usize;
    // A Graph Generic operation and its LoadWindow result share one source PC.
    // The collecting edge, rather than the number of SSA instruction regions,
    // selects the region that must publish the source and its live roots.
    let helper_regions: Vec<_> = regions
        .iter()
        .filter(|region| {
            region["startOffset"].as_u64().unwrap() <= helper as u64
                && (helper as u64) < region["endOffset"].as_u64().unwrap()
        })
        .collect();
    assert_eq!(
        helper_regions.len(),
        1,
        "the scalar edge has one source owner"
    );
    let start = helper_regions[0]["startOffset"].as_u64().unwrap() as usize;
    let code = bundle.file(JitArtifactFileName::Code).unwrap().contents();
    let pc_offset = (std::mem::offset_of!(otter_vm::native_abi::Frame, header)
        + std::mem::offset_of!(otter_vm::native_abi::VmFrameHeader, pc)) as u32;
    #[cfg(target_arch = "aarch64")]
    let stamps: Vec<_> = code[start..helper]
        .chunks_exact(4)
        .map(|word| u32::from_le_bytes(word.try_into().unwrap()))
        .collect::<Vec<_>>()
        .windows(2)
        .filter_map(|pair| {
            let register = pair[0] & 31;
            let movz = pair[0] & 0x7fe0_0000 == 0x5280_0000;
            let movn = pair[0] & 0xffe0_0000 == 0x1280_0000;
            let store = pair[1] & 0xffc0_03ff == (0xb900_02a0 | register);
            ((movz || movn) && store).then(|| {
                let value = (pair[0] >> 5) & 0xffff;
                (
                    ((pair[1] >> 10) & 0xfff) * 4,
                    if movn { !value } else { value },
                )
            })
        })
        .collect();
    #[cfg(target_arch = "x86_64")]
    let stamps: Vec<_> = native_code::decode(code, start, helper)
        .iter()
        .filter_map(|instruction| {
            let value = match instruction.operand(1) {
                Operand::ImmediateI32 { imm } => imm as u32,
                Operand::ImmediateU32 { imm } => imm,
                _ => return None,
            };
            let (base, offset) = native_code::base_offset(instruction.operand(0))?;
            (instruction.opcode() == Opcode::MOV
                && instruction.mem_size().and_then(|size| size.bytes_size()) == Some(4)
                && base == RegSpec::r14())
            .then_some((offset as u32, value))
        })
        .collect();
    assert!(
        stamps.contains(&(pc_offset, u32::try_from(logical_pc).unwrap())),
        "the active helper publishes the factory's exact NewError PC before entry"
    );
    let safepoints = json(bundle, JitArtifactFileName::Safepoints);
    // An active outer C helper reads the canonical frame PC. callPc describes
    // a suspended JS return edge or an inline descendant; an unrelated source
    // record does not prove which root record the active helper publishes.
    if bundle.manifest().tier() == JitDebugTier::Optimizing {
        let id = stamps
            .iter()
            .find_map(|&(offset, value)| {
                (offset == otter_vm::native_abi::NATIVE_FRAME_CALL_SITE_OFFSET).then_some(value)
            })
            .expect("the Graph helper publishes its exact live-root record");
        assert!(
            safepoints["records"]
                .as_array()
                .unwrap()
                .iter()
                .any(|record| {
                    record["id"].as_u64() == Some(u64::from(id))
                        && record["callPc"].is_null()
                        && record["inlineFrames"].as_array().unwrap().is_empty()
                })
        );
    }
}

#[test]
fn retained_implicit_errors_keep_their_source_realm_after_collection() {
    const ERROR_MODULE: &str = "implicit-error-source.js";
    const ERROR_SOURCE: &str = r#"
function implicitTypeErrorFactory(value) {
  try {
    const {marker} = value;
    if (marker < 0) return implicitTypeErrorFactory.apply(null, arguments);
    return marker;
  } catch (error) { return Object.getPrototypeOf(error); }
}
const warmErrorInput = {marker: 7};
for (let warm = 0; warm < 10000; warm++) implicitTypeErrorFactory(warmErrorInput);
saveIntrinsicBundle([implicitTypeErrorFactory, TypeError.prototype]);
"#;
    for selection in [
        JitSelection::InterpreterOnly,
        JitSelection::Template,
        JitSelection::ProductionTiered,
    ] {
        for default_source in [true, false] {
            let slot = Arc::new(Mutex::new(None));
            let mut runtime = Runtime::builder()
                .jit_compile_threads(0)
                .jit_selection(selection)
                .jit_debug(JitDebugRequest::artifacts().with_events(true))
                .extension_installer(exchange_installer(slot))
                .build()
                .unwrap();
            let source_realm = (!default_source).then(|| runtime.create_realm().unwrap());
            let setup = run(&mut runtime, source_realm, ERROR_SOURCE, ERROR_MODULE);
            if selection != JitSelection::InterpreterOnly {
                let expected_tier = match selection {
                    JitSelection::Template => JitDebugTier::Template,
                    JitSelection::ProductionTiered => JitDebugTier::Optimizing,
                    _ => unreachable!(),
                };
                let own: Vec<_> = setup
                    .jit_artifacts()
                    .expect("implicit-error artifacts")
                    .bundles()
                    .iter()
                    .filter(|bundle| {
                        bundle.manifest().module() == ERROR_MODULE
                            && bundle.manifest().function_name() == "implicitTypeErrorFactory"
                            && bundle.manifest().tier() == expected_tier
                    })
                    .collect();
                assert!(
                    !own.is_empty(),
                    "the retained factory must earn its own tier: {selection:?}"
                );
                for bundle in own {
                    assert_implicit_error_source(bundle);
                }
            }
            drop(setup);
            let caller_realm = default_source.then(|| runtime.create_realm().unwrap());
            run(
                &mut runtime,
                caller_realm,
                r#"
const retainedErrorBundle = loadIntrinsicBundle();
const callerErrorPrototype = TypeError.prototype;
const callerOnlyErrorPool = ["RangeError", "SyntaxError", "URIError", "caller-a", "caller-b"];
"#,
                "implicit-error-caller.js",
            );
            runtime
                .force_gc()
                .expect("move the retained factory and its canonical error prototype");
            let result = run(
                &mut runtime,
                caller_realm,
                r#"
const caughtNullPrototype = retainedErrorBundle[0](null);
const caughtUndefinedPrototype = retainedErrorBundle[0](undefined);
JSON.stringify([caughtNullPrototype === retainedErrorBundle[1],
 caughtUndefinedPrototype === retainedErrorBundle[1],
 caughtNullPrototype !== callerErrorPrototype,
 caughtUndefinedPrototype !== callerErrorPrototype,
 callerOnlyErrorPool.join("|") === "RangeError|SyntaxError|URIError|caller-a|caller-b"]);
"#,
                "implicit-error-probe.js",
            );
            assert_eq!(
                result.completion_string(),
                "[true,true,true,true,true]",
                "{selection:?}, default source: {default_source}"
            );
            drop(result);
            runtime
                .force_gc()
                .expect("retain returned implicit error prototypes in the caller");
            assert_eq!(
                run(
                    &mut runtime,
                    caller_realm,
                    r#"
JSON.stringify([caughtNullPrototype === retainedErrorBundle[1],
 caughtUndefinedPrototype === retainedErrorBundle[1],
 caughtNullPrototype !== callerErrorPrototype,
 caughtUndefinedPrototype !== callerErrorPrototype]);
"#,
                    "implicit-error-retained.js",
                )
                .completion_string(),
                "[true,true,true,true]",
                "{selection:?}, default source: {default_source}"
            );
        }
    }
}
