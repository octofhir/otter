//! Regression coverage for optimizing primitive-string intrinsics.
//!
//! # Contents
//! - Direct `charCodeAt(Int32)` and single-unit `indexOf(String)` completion.
//! - Latin-1 / UTF-16 inline and sequential bodies.
//! - Canonical fallbacks for ropes, slices, coercion, long search, and method
//!   replacement after tier-up.
//! - Address-stable literal cells surviving cache growth and moving GC.
//! - Artifact proof that supported calls do not retain their Rust leaf ABI.
//!
//! # Invariants
//! - Optimizing results match the interpreter oracle exactly.
//! - Speculative checks run before effects and a miss remains reusable.
//! - Supported generated operations contain no relocation to the replaced
//!   string leaf.
//! - Literal relocations name traced cells, never moving string handles.

use otter_runtime::{JitSelection, Runtime, SourceInput};

const STRING_MATRIX: &str = r#"
function codeAt(text, index) {
  return text.charCodeAt(index | 0);
}

function find(text, needle) {
  return text.indexOf(needle);
}

function findLiteral(text) {
  return text.indexOf("p");
}

for (let warm = 0; warm < 4010; warm++) {
  codeAt("alpha", warm & 3);
  find("alpha", "p");
  findLiteral("alpha");
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
  find("abc", "b"),
  findLiteral("alpha")
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
        r#"[[98,937,122,957,true,true,101,103,1,1,25,12,-1,2,25,3,23,300,-1,1],702,901,97,1,2]"#
    );
    #[cfg(target_arch = "aarch64")]
    assert!(
        optimized_entries > 0,
        "fixture must enter optimized string callers before replacement"
    );
}

#[cfg(target_arch = "aarch64")]
#[test]
fn optimized_literal_cell_survives_cache_rehash_and_full_gc() {
    let mut runtime = Runtime::builder()
        .jit_selection(JitSelection::ProductionTiered)
        .jit_osr_threshold(4)
        .build()
        .expect("literal-cell runtime");
    let before = runtime
        .run_script(
            SourceInput::from_javascript(
                r#"
                const literalWords = ["engine", "runtime"];
                function literalCellLoop(limit) {
                  let checksum = 0;
                  for (let index = 0; index < limit; index++) {
                    checksum += literalWords[index & 1].indexOf("e");
                  }
                  return checksum;
                }
                for (let warm = 0; warm < 4010; warm++) literalCellLoop(16);
                literalCellLoop(1024);
                "#,
            ),
            "literal-cell-warm.js",
        )
        .expect("warm literal loop")
        .completion_string()
        .to_owned();
    assert!(
        runtime.execution_stats().jit_optimized_entries > 0,
        "literal loop must enter optimized code before cache growth"
    );

    runtime
        .run_script(
            SourceInput::from_javascript(
                r#"
                for (let index = 0; index < 300; index++) {
                  eval("'literal-cell-churn-" + index + "'");
                }
                "#,
            ),
            "literal-cell-churn.js",
        )
        .expect("grow literal cache");
    runtime.force_gc().expect("full literal-cell GC");
    let after = runtime
        .run_script(
            SourceInput::from_javascript("literalCellLoop(1024)"),
            "literal-cell-after-gc.js",
        )
        .expect("reuse optimized literal loop")
        .completion_string()
        .to_owned();
    assert_eq!(after, before);
    assert_eq!(after, "3072");
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

    assert!(
        optimizing.len() >= 2,
        "both parameter string callers must optimize"
    );
    let optimizing_relocations = optimizing
        .iter()
        .filter_map(|bundle| bundle.file(JitArtifactFileName::Relocations))
        .map(|file| std::str::from_utf8(file.contents()).expect("relocations are UTF-8"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !optimizing_relocations.contains("string_char_code_at_leaf"),
        "charCodeAt must complete without the Rust leaf ABI: {optimizing_relocations}"
    );
    assert!(
        !optimizing_relocations.contains("string_index_of_leaf"),
        "indexOf must complete without the Rust leaf ABI: {optimizing_relocations}"
    );
    let all_relocations = artifacts
        .bundles()
        .iter()
        .filter_map(|bundle| bundle.file(JitArtifactFileName::Relocations))
        .map(|file| std::str::from_utf8(file.contents()).expect("relocations are UTF-8"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        all_relocations.contains("stringConstantCell"),
        "compiled string literals must load from typed traced cells: {all_relocations}"
    );
}
