//! Capture the actual shared machine-code owners referenced by a function.
//!
//! # Contents
//! - Exact routine bytes, canonical entry offsets and symbolic relocations.
//! - Portable normalized code using the ordinary relocation validator.
//!
//! # Invariants
//! - Each physical object is captured once, with all of its actual entries.
//! - Only objects referenced by this function's relocations are included.
//! - Captures borrow the retained placed mapping; no code is re-emitted.
//! - This renderer is called only for an explicitly requested artifact bundle.
//! - Routine offsets are local to that object, independent of `code.bin`.
//!
//! # See also
//! - `crate::routines` owns mappings and their immutable relocation records.
//! - `super::relocation` validates and normalizes the captured bytes.

use serde::Serialize;

use super::relocation::RelocationCapture;
use crate::{CompiledCode, routines::Routine};

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Entry {
    name: &'static str,
    offset: usize,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Object<'a> {
    name: &'static str,
    runtime_address_range: super::RuntimeAddressRange,
    entries: Vec<Entry>,
    code: &'a [u8],
    normalized_code: Vec<u8>,
    relocations: serde_json::Value,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Document<'a> {
    exact_code_is_runtime_local: bool,
    objects: Vec<Object<'a>>,
}

pub(super) fn render(code: &CompiledCode, references: &RelocationCapture) -> Option<String> {
    let mut required: std::collections::BTreeSet<_> = references.referenced_routines().collect();
    if required.is_empty() {
        return None;
    }
    let routines = code
        .routines()
        .expect("referenced routine owners are retained");
    let mut objects = Vec::new();
    for (kind, owner) in routines.code_objects() {
        let bytes = owner.code.bytes();
        let entries: Vec<_> = Routine::ALL
            .into_iter()
            .filter(|routine| routine.object() == kind)
            .map(|routine| {
                let address = if routine.second() {
                    owner.second
                } else {
                    owner.entry
                };
                let offset = address
                    .checked_sub(owner.entry)
                    .and_then(|v| usize::try_from(v).ok())
                    .and_then(|v| v.checked_add(owner.code.entry_offset()))
                    .expect("placed routine entry belongs to its retained mapping");
                assert!(offset < bytes.len());
                Entry {
                    name: routine.name(),
                    offset,
                }
            })
            .collect();
        if !entries.iter().any(|entry| required.contains(entry.name)) {
            continue;
        }
        for entry in &entries {
            required.remove(entry.name);
        }
        let rendered = owner
            .relocations
            .render(bytes)
            .unwrap_or_else(|error| panic!("invalid placed routine relocations: {error}"));
        objects.push(Object {
            name: kind.name(),
            runtime_address_range: {
                let start = usize::try_from(owner.entry)
                    .ok()
                    .and_then(|entry| entry.checked_sub(owner.code.entry_offset()))
                    .expect("placed routine base fits the native address space");
                super::RuntimeAddressRange::new(
                    start,
                    start.checked_add(bytes.len()).unwrap(),
                    owner.code.entry_offset(),
                )
            },
            entries,
            code: bytes,
            normalized_code: rendered.normalized_code,
            relocations: serde_json::from_str(&rendered.json)
                .expect("validated routine relocation document"),
        });
    }
    assert!(
        required.is_empty(),
        "missing referenced routine owners: {required:?}"
    );
    Some(
        serde_json::to_string_pretty(&Document {
            exact_code_is_runtime_local: true,
            objects,
        })
        .expect("serializing owned routine artifacts cannot fail"),
    )
}
