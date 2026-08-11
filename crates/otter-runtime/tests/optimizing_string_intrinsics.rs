//! Regression coverage for optimizing primitive-string intrinsics.
//!
//! # Contents
//! - Direct `charCodeAt(Int32)` and single-unit `indexOf(String)` completion.
//! - Latin-1 / UTF-16 inline and sequential bodies.
//! - Canonical fallbacks for ropes, slices, coercion, long search, and method
//!   replacement after tier-up.
//! - Artifact proof that supported calls do not retain their Rust leaf ABI.
//!
//! # Invariants
//! - Optimizing results match the interpreter oracle exactly.
//! - Speculative checks run before effects and a miss remains reusable.
//! - Supported generated operations contain no relocation to the replaced
//!   string leaf.

use otter_runtime::{JitSelection, Runtime, SourceInput};

const STRING_MATRIX: &str = r#"
function codeAt(text, index) {
  return text.charCodeAt(index | 0);
}

function find(text, needle) {
  return text.indexOf(needle);
}

for (let warm = 0; warm < 4010; warm++) {
  codeAt("alpha", warm & 3);
  find("alpha", "p");
}

const sequentialLatin1 = "abcdefghijklmnopqrstuvwxyz0123456789";
const sequentialWide = "αβγδεζηθικλμνξοπρστυφχψω";
function join(left, right) { return left + right; }
const rope = join(sequentialLatin1, sequentialLatin1);
const sliced = sequentialLatin1.slice(3, 30);
let coercions = 0;
const coerciveNeedle = {
  toString() {
    coercions += 1;
    return "z";
  }
};

const beforeReplacement = [
  codeAt("abc", 1),
  codeAt("aΩb", 1),
  codeAt(sequentialLatin1, 25),
  codeAt(sequentialWide, 12),
  Number.isNaN(codeAt("abc", -1)),
  Number.isNaN(codeAt("abc", 99)),
  codeAt(rope, 40),
  codeAt(sliced, 3),
  find("abc", "b"),
  find("aΩb", "Ω"),
  find(sequentialLatin1, "z"),
  find(sequentialWide, "ν"),
  find("abc", "Ω"),
  find("aΩb", "b"),
  find(rope, "z"),
  find(sliced, "g"),
  find(sequentialLatin1, "xyz"),
  find("a".repeat(300) + "z", "z"),
  find("abc", coerciveNeedle),
  coercions
];

const originalCodeAt = String.prototype.charCodeAt;
String.prototype.charCodeAt = function(index) { return 700 + index; };
const replacedCodeAt = codeAt("abc", 2);
String.prototype.charCodeAt = originalCodeAt;

const originalIndexOf = String.prototype.indexOf;
String.prototype.indexOf = function() { return 901; };
const replacedIndexOf = find("abc", "b");
String.prototype.indexOf = originalIndexOf;

JSON.stringify([
  beforeReplacement,
  replacedCodeAt,
  replacedIndexOf,
  codeAt("abc", 0),
  find("abc", "b")
]);
"#;

fn run(selection: JitSelection) -> (String, u64) {
    let mut runtime = Runtime::builder()
        .jit_selection(selection)
        .jit_osr_threshold(4)
        .build()
        .expect("string intrinsic runtime");
    let completion = runtime
        .run_script(
            SourceInput::from_javascript(STRING_MATRIX),
            "optimizing-string-intrinsics.js",
        )
        .expect("string intrinsic matrix")
        .completion_string()
        .to_owned();
    (completion, runtime.execution_stats().jit_optimized_entries)
}

#[test]
fn optimizing_string_intrinsics_match_oracle_and_preserve_fallbacks() {
    let (oracle, _) = run(JitSelection::InterpreterOnly);
    let (compiled, optimized_entries) = run(JitSelection::ProductionTiered);

    assert_eq!(compiled, oracle);
    assert_eq!(
        oracle,
        r#"[[98,937,122,957,true,true,101,103,1,1,25,12,-1,2,25,3,23,300,-1,1],702,901,97,1]"#
    );
    #[cfg(target_arch = "aarch64")]
    assert!(
        optimized_entries > 0,
        "fixture must enter optimized string callers before replacement"
    );
}

#[cfg(target_arch = "aarch64")]
#[test]
fn optimizing_string_artifacts_have_no_replaced_leaf_relocations() {
    use otter_runtime::{JitArtifactFileName, JitDebugRequest, JitDebugTier};

    let mut runtime = Runtime::builder()
        .jit_selection(JitSelection::ProductionTiered)
        .jit_osr_threshold(4)
        .jit_debug(JitDebugRequest::artifacts())
        .build()
        .expect("string artifact runtime");
    let result = runtime
        .run_script(
            SourceInput::from_javascript(STRING_MATRIX),
            "optimizing-string-artifacts.js",
        )
        .expect("string artifact matrix");
    let artifacts = result.jit_artifacts().expect("enabled artifact batch");
    let optimizing: Vec<_> = artifacts
        .bundles()
        .iter()
        .filter(|bundle| bundle.manifest().tier() == JitDebugTier::Optimizing)
        .collect();

    assert!(optimizing.len() >= 2, "both string callers must optimize");
    let relocations = optimizing
        .iter()
        .filter_map(|bundle| bundle.file(JitArtifactFileName::Relocations))
        .map(|file| std::str::from_utf8(file.contents()).expect("relocations are UTF-8"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !relocations.contains("string_char_code_at_leaf"),
        "charCodeAt must complete without the Rust leaf ABI: {relocations}"
    );
    assert!(
        !relocations.contains("string_index_of_leaf"),
        "indexOf must complete without the Rust leaf ABI: {relocations}"
    );
}
