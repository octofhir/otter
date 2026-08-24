//! Source-level guards for the opaque in-process snapshot boundary.
//!
//! # Contents
//! - [`serialized_raw_snapshot_entry_points_are_absent`] locks out the three
//!   removed byte-admission functions.
//!
//! # Invariants
//! - Raw GC pages and VM snapshots have no byte decoder.
//! - Flat bytecode decoding admits only base-zero cache modules; rebased
//!   snapshot chunks cannot be reconstructed from bytes.
//!
//! # See also
//! - `tests/compile_fail/isolate_snapshot_raw_field_rejected.rs` proves the
//!   carrier's raw image cannot be replaced by external safe code.

#[test]
fn serialized_raw_snapshot_entry_points_are_absent() {
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let heap_image = std::fs::read_to_string(manifest.join("../otter-gc/src/heap_image.rs"))
        .expect("read heap-image source");
    let isolate_snapshot = std::fs::read_to_string(manifest.join("src/snapshot.rs"))
        .expect("read isolate-snapshot source");
    let bytecode_binary = std::fs::read_to_string(manifest.join("../otter-bytecode/src/binary.rs"))
        .expect("read bytecode-binary source");

    assert!(!heap_image.contains("pub fn from_bytes("));
    assert!(!isolate_snapshot.contains("pub fn from_bytes("));
    assert!(!bytecode_binary.contains("pub fn decode_module_at_base("));
}
