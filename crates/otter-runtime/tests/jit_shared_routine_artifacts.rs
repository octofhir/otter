//! Owned snapshots of the actual shared code referenced by a native body.
//!
//! # Contents
//! - Later callers reuse the first placed routine, including its relocations.
//! - Captures exclude unrelated cached objects and remain valid after GC/drop.
//! - Independent live mappings have identical portable normalized code.
//!
//! # Invariants
//! - Entry offsets name the captured routine object, never the caller's code.
//! - Each physical owner occurs once, even when it has several entry aliases.
//! - Exact runtime-local code and portable normalized code stay distinct.
//!
//! # See also
//! - `otter-jit/src/artifact/routines.rs` owns routine capture.
//! - `jit_known_native_calls/mixed_get.rs` joins real Get/GC/OOM behavior.

use otter_runtime::{JitArtifactFileName, JitDebugRequest, JitSelection, Runtime, SourceInput};
use serde_json::Value;

fn capture(runtime: &mut Runtime, source: &str, name: &str) -> Value {
    let result = runtime
        .run_script(SourceInput::from_javascript(source), name)
        .unwrap();
    let batch = result.jit_artifacts().unwrap();
    assert!(!batch.truncated());
    let own: Vec<_> = batch
        .bundles()
        .iter()
        .filter(|bundle| {
            bundle.manifest().function_name() == name && bundle.manifest().module() == name
        })
        .collect();
    assert_eq!(own.len(), 1, "one exact own source bundle");
    serde_json::from_slice(
        own[0]
            .file(JitArtifactFileName::Routines)
            .expect("actual shared owners")
            .contents(),
    )
    .unwrap()
}

fn validate(document: &Value) {
    assert_eq!(document["exactCodeIsRuntimeLocal"], true);
    let objects = document["objects"].as_array().unwrap();
    assert_eq!(
        objects.len(),
        1,
        "only the object this source actually calls"
    );
    let object = &objects[0];
    assert_eq!(object["name"], "routine:load");
    let bytes = object["code"].as_array().unwrap();
    assert!(!bytes.is_empty());
    let address = |field: &str| {
        usize::from_str_radix(
            object["runtimeAddressRange"][field]
                .as_str()
                .unwrap()
                .strip_prefix("0x")
                .unwrap(),
            16,
        )
        .unwrap()
    };
    assert_eq!(address("endExclusive") - address("start"), bytes.len());
    assert!(address("start") <= address("entry") && address("entry") < address("endExclusive"));
    let entries = object["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 2, "one owner for load and probeLoad");
    let mut names = std::collections::BTreeSet::new();
    for entry in entries {
        assert!(names.insert(entry["name"].as_str().unwrap()));
        assert!(entry["offset"].as_u64().unwrap() < bytes.len() as u64);
    }
    assert_eq!(names, ["load", "probeLoad"].into_iter().collect());
    let normalized: Vec<u8> = object["normalizedCode"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| u8::try_from(value.as_u64().unwrap()).unwrap())
        .collect();
    assert!(normalized.starts_with(b"OTJNCODE"));
    let relocations = &object["relocations"]["relocations"];
    assert!(relocations.as_array().unwrap().iter().any(|relocation| {
        relocation["target"]["kind"] == "runtimeStub"
            && relocation["target"]["name"] == "jit_load_property_value"
    }));
    assert!(!object["relocations"].to_string().contains("resolvedValue"));
}

#[test]
fn later_capture_keeps_the_same_placed_owner_and_excludes_unrelated_routines() {
    let mut runtime = Runtime::builder()
        .jit_compile_threads(0)
        .jit_selection(JitSelection::Template)
        .jit_debug(JitDebugRequest::artifacts())
        .build()
        .unwrap();
    let first = capture(
        &mut runtime,
        r#"
function firstSharedLoad(value) { return value.payload; }
const sharedLoadInput = new Proxy({payload: 719}, {get(t,k,r){return Reflect.get(t,k,r);}});
for(let warm=0;warm<5000;warm++) firstSharedLoad(sharedLoadInput);
"#,
        "firstSharedLoad",
    );
    validate(&first);
    let unrelated = runtime
        .run_script(
            SourceInput::from_javascript(
                r#"
function unrelatedSharedStore(value, item) { value.payload = item; }
const sharedStoreInput = new Proxy({}, {set(t,k,v){t[k]=v;return true;}});
for(let warm=0;warm<5000;warm++) unrelatedSharedStore(sharedStoreInput, warm);
"#,
            ),
            "unrelatedSharedStore",
        )
        .unwrap();
    assert!(
        unrelated
            .jit_artifacts()
            .unwrap()
            .bundles()
            .iter()
            .any(|bundle| {
                bundle
                    .file(JitArtifactFileName::Routines)
                    .is_some_and(|file| {
                        let document: Value = serde_json::from_slice(file.contents()).unwrap();
                        document["objects"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .any(|object| object["name"] == "routine:store")
                    })
            }),
        "an unrelated physical owner is actually populated"
    );
    runtime.force_gc().unwrap();
    let later = capture(
        &mut runtime,
        r#"
function laterSharedLoad(value) { return value.payload; }
for(let warm=0;warm<5000;warm++) laterSharedLoad(sharedLoadInput);
"#,
        "laterSharedLoad",
    );
    validate(&later);
    assert_eq!(
        first, later,
        "actual mapping, bytes, entries and relocations are reused"
    );
    drop(runtime);
    validate(&first);
    validate(&later);
}

#[test]
fn independent_placements_keep_portable_routine_identity() {
    let runtime = || {
        Runtime::builder()
            .jit_compile_threads(0)
            .jit_selection(JitSelection::Template)
            .jit_debug(JitDebugRequest::artifacts())
            .build()
            .unwrap()
    };
    let mut first_runtime = runtime();
    let mut second_runtime = runtime();
    let source = r#"
function portableSharedLoad(value) { return value.payload; }
const portableInput = new Proxy({payload: 719}, {get(t,k,r){return Reflect.get(t,k,r);}});
for(let warm=0;warm<5000;warm++) portableSharedLoad(portableInput);
"#;
    let first = capture(&mut first_runtime, source, "portableSharedLoad");
    let second = capture(&mut second_runtime, source, "portableSharedLoad");
    validate(&first);
    validate(&second);
    let first = &first["objects"][0];
    let second = &second["objects"][0];
    assert_ne!(
        first["runtimeAddressRange"], second["runtimeAddressRange"],
        "two live runtimes own distinct physical mappings"
    );
    for field in ["name", "entries", "normalizedCode", "relocations"] {
        assert_eq!(first[field], second[field], "portable routine {field}");
    }
    drop((first_runtime, second_runtime));
}
