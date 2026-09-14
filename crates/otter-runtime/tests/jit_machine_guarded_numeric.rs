//! Machine IR guarded-number coverage for cold numeric operations and mixed phis.
//!
//! # Contents
//! - A compact Navier-shaped packed-double loop whose element guards, address
//!   derivation, load/store effects, and committed cold siblings are explicit.
//! - Arithmetic and relational operations that remain unseen until after the
//!   Machine body is published, including exact object/Symbol deoptimization.
//! - A baseline-prepared element family whose still-unseen multiplication
//!   supplies a Number index, including exact-Uint32 proof and a committed
//!   fractional cold completion.
//! - A packed-double element family whose dynamic index remains tagged,
//!   including direct int32 hits and exact string/fractional misses.
//! - A Delta-shaped constructor whose `initialValue || 0` join deliberately
//!   retains its Template baseline while field transitions stay live.
//!
//! # Invariants
//! - Element receiver, bounds/address, representation, load/store, and cold
//!   effects remain separate Machine operations. Empty-feedback numeric
//!   parameter sites still use guarded decodes rather than a cold-value exit.
//! - Numeric inputs stay generated; a coercive cell exits at its original
//!   operation exactly once, and already-committed element effects never replay.
//! - A packed-array Float64 index is checked at each indexed use. Exact Uint32
//!   values, including negative zero as property key `"0"`, remain generated;
//!   fractional values use the committed canonical sibling without replay.
//! - A tagged packed-array index proves an exact index in generated code;
//!   every other property-key representation enters that same cold sibling.
//! - Constructor field-transition lowering does not force a heterogeneous
//!   tagged/value join into Machine IR; rejection retains the Template body.
//!
//! # See also
//! - `crates/otter-jit/src/machine/numeric` owns guarded numeric HIR,
//!   representation-changing phi edges, selection, and AArch64 emission.

#![cfg(target_arch = "aarch64")]

use otter_runtime::{
    JitArtifactBatch, JitArtifactBundle, JitArtifactFileName, JitDebugRequest, JitDebugTarget,
    JitDebugTier, JitSelection, Runtime, RuntimeExecutionStats, SourceInput,
};

const MACHINE_IR_HEADER: &[u8] = b"; backend=otter-machine-ir scalar-function\n";
const NAVIER_MODULE: &str = "jit-machine-guarded-numeric-navier-setup.js";
const PACKED_NAVIER_MODULE: &str = "jit-machine-packed-double-navier-setup.js";
const NUMBER_INDEX_MODULE: &str = "jit-machine-guarded-numeric-number-index-setup.js";
const TAGGED_INDEX_MODULE: &str = "jit-machine-guarded-numeric-tagged-index-setup.js";
const DELTA_MODULE: &str = "jit-machine-guarded-numeric-delta-setup.js";

const NAVIER_SETUP: &str = r#"
function machineGuardedNavier(values, effects, takeGuarded, divisor, threshold) {
  let total = values[0];
  for (let index = 1; index < 4; index = index + 1) {
    const current = values[index];
    total = total + current;
    values[index] = current;
  }

  effects[0] = effects[0] + 1;
  if (takeGuarded) {
    if (total > threshold) {
      return total / divisor;
    }
    return 0;
  }
  return total;
}

globalThis.__guardedNavierValues = [0.5, 1.5, 2.5, 3.5];
globalThis.__guardedNavierEffects = [0];
for (let warm = 0; warm < 5000; warm++) {
  machineGuardedNavier(
    __guardedNavierValues,
    __guardedNavierEffects,
    false,
    2,
    1
  );
}
__guardedNavierEffects[0] = 0;
"#;

const NAVIER_NUMERIC_PROBE: &str = r#"
const __guardedNumericBefore = __guardedNavierEffects[0];
const __guardedNumericResult = machineGuardedNavier(
  __guardedNavierValues,
  __guardedNavierEffects,
  true,
  2,
  1
);
JSON.stringify([
  __guardedNumericResult,
  __guardedNavierEffects[0] - __guardedNumericBefore
]);
"#;

const NAVIER_OBJECT_PROBE: &str = r#"
globalThis.__guardedObjectCoercions = 0;
const __guardedThreshold = {
  [Symbol.toPrimitive]() {
    __guardedObjectCoercions++;
    return 1;
  }
};
const __guardedObjectBefore = __guardedNavierEffects[0];
const __guardedObjectResult = machineGuardedNavier(
  __guardedNavierValues,
  __guardedNavierEffects,
  true,
  2,
  __guardedThreshold
);
JSON.stringify([
  __guardedObjectResult,
  __guardedObjectCoercions,
  __guardedNavierEffects[0] - __guardedObjectBefore
]);
"#;

const NAVIER_SYMBOL_PROBE: &str = r#"
let __guardedSymbolError = "";
const __guardedSymbolBefore = __guardedNavierEffects[0];
try {
  machineGuardedNavier(
    __guardedNavierValues,
    __guardedNavierEffects,
    true,
    Symbol("guarded divisor"),
    1
  );
} catch (error) {
  __guardedSymbolError = error.name;
}
JSON.stringify([
  __guardedSymbolError,
  __guardedNavierEffects[0] - __guardedSymbolBefore
]);
"#;

const NAVIER_REUSE_PROBE: &str = r#"
const __guardedReuseBefore = __guardedNavierEffects[0];
const __guardedReuseResult = machineGuardedNavier(
  __guardedNavierValues,
  __guardedNavierEffects,
  true,
  4,
  1
);
JSON.stringify([
  __guardedReuseResult,
  __guardedNavierEffects[0] - __guardedReuseBefore
]);
"#;

const PACKED_NAVIER_SETUP: &str = r#"
function machinePackedDoubleNavier(input, output) {
  for (let index = 1; index < 4; index = index + 1) {
    const sum = input[index - 1] + input[index] + input[index + 1];
    output[index] = sum * 0.25;
  }
  return output;
}

globalThis.__packedNavierInput = [0.5, 1.5, 2.5, 3.5, 4.5];
globalThis.__packedNavierOutput = [0, 0, 0, 0, 0];
for (let warm = 0; warm < 5000; warm++) {
  machinePackedDoubleNavier(__packedNavierInput, __packedNavierOutput);
}
"#;

const PACKED_NAVIER_PROBE: &str = r#"
globalThis.__packedNavierFinalInput = [0.5, 1.5, 2.5, 3.5, 4.5];
globalThis.__packedNavierFinalOutput = [0, 0, 0, 0, 0];
machinePackedDoubleNavier(
  __packedNavierFinalInput,
  __packedNavierFinalOutput
);
JSON.stringify(__packedNavierFinalOutput);
"#;

const NUMBER_INDEX_SETUP: &str = r#"
function machineGuardedNumberIndex(values, effects, takeElements, left, right) {
  if (!takeElements) {
    return 0;
  }

  const index = left * right;
  effects[0] = effects[0] + 1;
  const loaded = values[index];
  values[index] = loaded + 1;
  return loaded;
}

globalThis.__guardedNumberIndexValues = [10, 20, 30, 40];
globalThis.__guardedNumberIndexEffects = [0];

// Publish baseline code while the multiplication and element sites are cold.
for (let warm = 0; warm < 64; warm++) {
  machineGuardedNumberIndex(
    __guardedNumberIndexValues,
    __guardedNumberIndexEffects,
    false,
    1,
    1
  );
}

// Baseline numeric code does not populate arithmetic feedback, but its generic
// element transitions prepare both indexed sites for the optimizing snapshot.
for (let warm = 0; warm < 32; warm++) {
  machineGuardedNumberIndex(
    __guardedNumberIndexValues,
    __guardedNumberIndexEffects,
    true,
    0.5,
    2
  );
}

// Let the prepared feedback stabilize before optimizing promotion.
for (let warm = 0; warm < 5000; warm++) {
  machineGuardedNumberIndex(
    __guardedNumberIndexValues,
    __guardedNumberIndexEffects,
    false,
    1,
    1
  );
}

globalThis.__guardedNumberIndexValues = [10, 20, 30, 40];
globalThis.__guardedNumberFractionalValues = [10, 20, 30, 40];
globalThis.__guardedNumberIndexEffects = [0];
"#;

const NUMBER_INDEX_INTEGRAL_PROBE: &str = r#"
const __guardedNumberIntegralBefore = __guardedNumberIndexEffects[0];
const __guardedNumberIntegralResult = machineGuardedNumberIndex(
  __guardedNumberIndexValues,
  __guardedNumberIndexEffects,
  true,
  1,
  2
);
JSON.stringify([
  __guardedNumberIntegralResult,
  __guardedNumberIndexValues[2],
  __guardedNumberIndexEffects[0] - __guardedNumberIntegralBefore
]);
"#;

const NUMBER_INDEX_FRACTIONAL_PROBE: &str = r#"
const __guardedNumberFractionalBefore = __guardedNumberIndexEffects[0];
const __guardedNumberFractionalResult = machineGuardedNumberIndex(
  __guardedNumberFractionalValues,
  __guardedNumberIndexEffects,
  true,
  0.5,
  1
);
JSON.stringify([
  __guardedNumberFractionalResult,
  Object.prototype.hasOwnProperty.call(__guardedNumberFractionalValues, "0.5"),
  Number.isNaN(__guardedNumberFractionalValues[0.5]),
  __guardedNumberIndexEffects[0] - __guardedNumberFractionalBefore
]);
"#;

const NUMBER_INDEX_NEGATIVE_ZERO_PROBE: &str = r#"
const __guardedNumberNegativeZeroBefore = __guardedNumberIndexEffects[0];
const __guardedNumberNegativeZeroResult = machineGuardedNumberIndex(
  __guardedNumberIndexValues,
  __guardedNumberIndexEffects,
  true,
  -0,
  1
);
JSON.stringify([
  __guardedNumberNegativeZeroResult,
  __guardedNumberIndexValues[0],
  __guardedNumberIndexEffects[0] - __guardedNumberNegativeZeroBefore
]);
"#;

const NUMBER_INDEX_REUSE_PROBE: &str = r#"
globalThis.__guardedNumberReuseValues = [10, 20, 30, 40];
const __guardedNumberReuseBefore = __guardedNumberIndexEffects[0];
const __guardedNumberReuseResult = machineGuardedNumberIndex(
  __guardedNumberReuseValues,
  __guardedNumberIndexEffects,
  true,
  1,
  3
);
JSON.stringify([
  __guardedNumberReuseResult,
  __guardedNumberReuseValues[3],
  __guardedNumberIndexEffects[0] - __guardedNumberReuseBefore
]);
"#;

const TAGGED_INDEX_SETUP: &str = r#"
function machineTaggedPackedDoubleIndex(values, effects, takeElements, index) {
  if (!takeElements) {
    return 0;
  }

  const loaded = values[index];
  effects.count = effects.count + 1;
  values[index] = loaded + 0.5;
  return loaded;
}

globalThis.__taggedPackedWarmValues = [10, 20, 30, 40];
globalThis.__taggedPackedEffects = { count: 0 };

// Publish baseline code before either indexed site has receiver feedback.
for (let warm = 0; warm < 64; warm++) {
  machineTaggedPackedDoubleIndex(
    __taggedPackedWarmValues,
    __taggedPackedEffects,
    false,
    1
  );
}

// Prepare both dense sites while the parameter itself remains a tagged value.
for (let warm = 0; warm < 32; warm++) {
  machineTaggedPackedDoubleIndex(
    __taggedPackedWarmValues,
    __taggedPackedEffects,
    true,
    1
  );
}

// Let the prepared feedback stabilize before optimizing promotion.
for (let warm = 0; warm < 5000; warm++) {
  machineTaggedPackedDoubleIndex(
    __taggedPackedWarmValues,
    __taggedPackedEffects,
    false,
    1
  );
}

globalThis.__taggedPackedEffects.count = 0;
"#;

const TAGGED_INDEX_INTEGRAL_PROBE: &str = r#"
globalThis.__taggedPackedIntegralValues = [10, 20, 30, 40];
const __taggedPackedIntegralBefore = __taggedPackedEffects.count;
const __taggedPackedIntegralResult = machineTaggedPackedDoubleIndex(
  __taggedPackedIntegralValues,
  __taggedPackedEffects,
  true,
  2
);
JSON.stringify([
  __taggedPackedIntegralResult,
  __taggedPackedIntegralValues[2],
  __taggedPackedEffects.count - __taggedPackedIntegralBefore
]);
"#;

const TAGGED_INDEX_STRING_PROBE: &str = r#"
globalThis.__taggedPackedStringValues = [10, 20, 30, 40];
const __taggedPackedStringBefore = __taggedPackedEffects.count;
const __taggedPackedStringResult = machineTaggedPackedDoubleIndex(
  __taggedPackedStringValues,
  __taggedPackedEffects,
  true,
  "1"
);
JSON.stringify([
  __taggedPackedStringResult,
  __taggedPackedStringValues[1],
  __taggedPackedEffects.count - __taggedPackedStringBefore
]);
"#;

const TAGGED_INDEX_FRACTIONAL_PROBE: &str = r#"
globalThis.__taggedPackedFractionalValues = [10, 20, 30, 40];
const __taggedPackedFractionalBefore = __taggedPackedEffects.count;
const __taggedPackedFractionalResult = machineTaggedPackedDoubleIndex(
  __taggedPackedFractionalValues,
  __taggedPackedEffects,
  true,
  0.5
);
JSON.stringify([
  __taggedPackedFractionalResult,
  Object.prototype.hasOwnProperty.call(__taggedPackedFractionalValues, "0.5"),
  Number.isNaN(__taggedPackedFractionalValues[0.5]),
  __taggedPackedEffects.count - __taggedPackedFractionalBefore
]);
"#;

const TAGGED_INDEX_REUSE_PROBE: &str = r#"
globalThis.__taggedPackedReuseValues = [10, 20, 30, 40];
const __taggedPackedReuseBefore = __taggedPackedEffects.count;
const __taggedPackedReuseResult = machineTaggedPackedDoubleIndex(
  __taggedPackedReuseValues,
  __taggedPackedEffects,
  true,
  3
);
JSON.stringify([
  __taggedPackedReuseResult,
  __taggedPackedReuseValues[3],
  __taggedPackedEffects.count - __taggedPackedReuseBefore
]);
"#;

const DELTA_SETUP: &str = r#"
function DeltaGuarded(initialValue) {
  const value = initialValue || 0;
  this.value = value;
  this.next = value;
  this.marker = "delta";
}

globalThis.__guardedDeltaLast = undefined;
for (let warm = 0; warm < 5000; warm++) {
  __guardedDeltaLast = new DeltaGuarded(
    (warm & 1) === 0 ? undefined : warm
  );
}
"#;

const DELTA_PROBE: &str = r#"
const __guardedDeltaZero = new DeltaGuarded(undefined);
const __guardedDeltaNumber = new DeltaGuarded(7);
const __guardedDeltaString = new DeltaGuarded("4");
JSON.stringify([
  [
    __guardedDeltaZero.value,
    __guardedDeltaZero.next,
    __guardedDeltaZero.marker,
    Object.keys(__guardedDeltaZero).join(",")
  ],
  [
    __guardedDeltaNumber.value,
    __guardedDeltaNumber.next,
    __guardedDeltaNumber.marker,
    Object.keys(__guardedDeltaNumber).join(",")
  ],
  [
    __guardedDeltaString.value,
    __guardedDeltaString.next,
    __guardedDeltaString.marker,
    Object.keys(__guardedDeltaString).join(",")
  ]
]);
"#;

#[derive(Debug, Clone, Copy)]
struct CounterDelta {
    optimized_entries: u64,
    optimized_deopts: u64,
}

impl CounterDelta {
    fn between(before: RuntimeExecutionStats, after: RuntimeExecutionStats) -> Self {
        Self {
            optimized_entries: after.jit_optimized_entries - before.jit_optimized_entries,
            optimized_deopts: after.jit_optimized_deopts - before.jit_optimized_deopts,
        }
    }
}

fn runtime(selection: JitSelection, artifacts: bool) -> Runtime {
    let builder = Runtime::builder().jit_selection(selection);
    if artifacts {
        builder.jit_debug(JitDebugRequest::artifacts()).build()
    } else {
        builder.build()
    }
    .expect("guarded numeric runtime")
}

fn completion(runtime: &mut Runtime, source: &str, module: &str) -> String {
    runtime
        .run_script(SourceInput::from_javascript(source), module)
        .unwrap_or_else(|error| panic!("guarded numeric fixture {module}: {error:?}"))
        .completion_string()
        .to_owned()
}

fn artifact_json(bundle: &JitArtifactBundle, file: JitArtifactFileName) -> serde_json::Value {
    serde_json::from_slice(
        bundle
            .file(file)
            .unwrap_or_else(|| panic!("missing {file:?} in {:?}", bundle.manifest()))
            .contents(),
    )
    .unwrap_or_else(|error| panic!("invalid {file:?} JSON: {error}"))
}

fn machine_navier_bundle(artifacts: &JitArtifactBatch) -> &JitArtifactBundle {
    artifacts
        .bundles()
        .iter()
        .find(|bundle| {
            let manifest = bundle.manifest();
            manifest.module() == NAVIER_MODULE
                && manifest.function_name() == "machineGuardedNavier"
                && manifest.tier() == JitDebugTier::Optimizing
                && manifest.entry() == JitDebugTarget::Entry
                && bundle
                    .file(JitArtifactFileName::OptimizedIr)
                    .is_some_and(|file| file.contents().starts_with(MACHINE_IR_HEADER))
        })
        .unwrap_or_else(|| {
            let manifests = artifacts
                .bundles()
                .iter()
                .map(|bundle| {
                    let manifest = bundle.manifest();
                    format!(
                        "{}:{}:{:?}:{:?}",
                        manifest.module(),
                        manifest.function_name(),
                        manifest.tier(),
                        manifest.entry()
                    )
                })
                .collect::<Vec<_>>();
            panic!("missing exact guarded Navier Machine bundle: {manifests:?}")
        })
}

fn packed_navier_bundle(artifacts: &JitArtifactBatch) -> &JitArtifactBundle {
    artifacts
        .bundles()
        .iter()
        .find(|bundle| {
            let manifest = bundle.manifest();
            manifest.module() == PACKED_NAVIER_MODULE
                && manifest.function_name() == "machinePackedDoubleNavier"
                && manifest.tier() == JitDebugTier::Optimizing
                && manifest.entry() == JitDebugTarget::Entry
                && bundle
                    .file(JitArtifactFileName::OptimizedIr)
                    .is_some_and(|file| file.contents().starts_with(MACHINE_IR_HEADER))
        })
        .unwrap_or_else(|| {
            let manifests = artifacts
                .bundles()
                .iter()
                .map(|bundle| {
                    let manifest = bundle.manifest();
                    format!(
                        "{}:{}:{:?}:{:?}",
                        manifest.module(),
                        manifest.function_name(),
                        manifest.tier(),
                        manifest.entry()
                    )
                })
                .collect::<Vec<_>>();
            panic!("missing exact packed Navier Machine bundle: {manifests:?}")
        })
}

fn machine_number_index_bundle(artifacts: &JitArtifactBatch) -> &JitArtifactBundle {
    artifacts
        .bundles()
        .iter()
        .find(|bundle| {
            let manifest = bundle.manifest();
            manifest.module() == NUMBER_INDEX_MODULE
                && manifest.function_name() == "machineGuardedNumberIndex"
                && manifest.tier() == JitDebugTier::Optimizing
                && manifest.entry() == JitDebugTarget::Entry
                && bundle
                    .file(JitArtifactFileName::OptimizedIr)
                    .is_some_and(|file| file.contents().starts_with(MACHINE_IR_HEADER))
        })
        .unwrap_or_else(|| {
            let manifests = artifacts
                .bundles()
                .iter()
                .map(|bundle| {
                    let manifest = bundle.manifest();
                    format!(
                        "{}:{}:{:?}:{:?}",
                        manifest.module(),
                        manifest.function_name(),
                        manifest.tier(),
                        manifest.entry()
                    )
                })
                .collect::<Vec<_>>();
            panic!("missing exact Number-index Machine bundle: {manifests:?}")
        })
}

fn machine_tagged_index_bundle(artifacts: &JitArtifactBatch) -> &JitArtifactBundle {
    artifacts
        .bundles()
        .iter()
        .find(|bundle| {
            let manifest = bundle.manifest();
            manifest.module() == TAGGED_INDEX_MODULE
                && manifest.function_name() == "machineTaggedPackedDoubleIndex"
                && manifest.tier() == JitDebugTier::Optimizing
                && manifest.entry() == JitDebugTarget::Entry
                && bundle
                    .file(JitArtifactFileName::OptimizedIr)
                    .is_some_and(|file| file.contents().starts_with(MACHINE_IR_HEADER))
        })
        .unwrap_or_else(|| {
            let manifests = artifacts
                .bundles()
                .iter()
                .map(|bundle| {
                    let manifest = bundle.manifest();
                    format!(
                        "{}:{}:{:?}:{:?}",
                        manifest.module(),
                        manifest.function_name(),
                        manifest.tier(),
                        manifest.entry()
                    )
                })
                .collect::<Vec<_>>();
            panic!("missing exact tagged-index Machine bundle: {manifests:?}")
        })
}

fn machine_values(line: &str) -> Vec<u32> {
    let mut values = Vec::new();
    let mut remaining = line;
    while let Some(start) = remaining.find("MachineValue(") {
        remaining = &remaining[start + "MachineValue(".len()..];
        let end = remaining
            .find(')')
            .unwrap_or_else(|| panic!("unterminated Machine value in: {line}"));
        values.push(
            remaining[..end]
                .parse()
                .unwrap_or_else(|error| panic!("invalid Machine value in {line}: {error}")),
        );
        remaining = &remaining[end + 1..];
    }
    values
}

fn machine_representation(optimized_ir: &str, value: u32) -> &str {
    let marker = format!("v{value}:");
    optimized_ir
        .lines()
        .find_map(|line| line.strip_prefix(&marker))
        .unwrap_or_else(|| panic!("missing representation for {marker} in: {optimized_ir}"))
}

fn assert_decomposed_element_flow(
    optimized_ir: &str,
    expected_loads: usize,
    expected_stores: usize,
) {
    let machine_ir = optimized_ir
        .split_once("allocation target=")
        .map_or(optimized_ir, |(machine_ir, _)| machine_ir);
    let lines = machine_ir.lines().collect::<Vec<_>>();
    let count = |opcode: &str| {
        let marker = format!(" {opcode} {{");
        lines.iter().filter(|line| line.contains(&marker)).count()
    };
    let sites = expected_loads + expected_stores;
    assert_eq!(
        count("ElementView"),
        sites,
        "one view proof per site: {optimized_ir}"
    );
    assert_eq!(
        count("ElementAddress"),
        sites,
        "one bounds/address proof per site: {optimized_ir}"
    );
    assert_eq!(
        count("ElementValueLoad"),
        expected_loads,
        "one direct load effect per load: {optimized_ir}"
    );
    assert_eq!(
        count("ElementValueGuard"),
        expected_stores,
        "one value proof per store: {optimized_ir}"
    );
    assert_eq!(
        count("ElementValueStore"),
        expected_stores,
        "one no-fail store effect per store: {optimized_ir}"
    );
    for line in lines
        .iter()
        .filter(|line| line.contains(" ElementValueLoad {"))
    {
        let values = machine_values(line);
        assert_eq!(
            machine_representation(machine_ir, values[2]),
            "Tagged",
            "the representation-specific load must publish a normal tagged SSA value: {line}"
        );
    }
    for forbidden in [
        "PackedDoubleElementLoad",
        "PackedDoubleElementStore",
        " ElementLoad(",
        " ElementStore(",
        "CheckedFloat64ToElementIndex",
    ] {
        assert!(
            !machine_ir.contains(forbidden),
            "decomposed element IR must not retain {forbidden}: {optimized_ir}"
        );
    }
}

fn element_byte_pcs(optimized_ir: &str, opcode: &str) -> Vec<u64> {
    let machine_ir = optimized_ir
        .split_once("allocation target=")
        .map_or(optimized_ir, |(machine_ir, _)| machine_ir);
    let opcode_marker = format!(" {opcode} {{ byte_pc: ");
    machine_ir
        .lines()
        .filter_map(|line| {
            line.split_once(&opcode_marker)
                .and_then(|(_, tail)| tail.split_once([',', ' ']))
                .and_then(|(byte_pc, _)| byte_pc.parse::<u64>().ok())
        })
        .collect()
}

fn instruction_byte_pc(bundle: &JitArtifactBundle, opcode: &str) -> u64 {
    let bytecode = std::str::from_utf8(
        bundle
            .file(JitArtifactFileName::Bytecode)
            .expect("guarded numeric bytecode artifact")
            .contents(),
    )
    .expect("UTF-8 guarded numeric bytecode artifact");
    let instruction = bytecode
        .lines()
        .find(|line| line.contains(&format!(" {opcode} ")))
        .unwrap_or_else(|| panic!("missing {opcode} in guarded numeric bytecode: {bytecode}"));
    instruction
        .split_whitespace()
        .find_map(|field| field.strip_prefix("byte="))
        .and_then(|pc| pc.parse::<u64>().ok())
        .unwrap_or_else(|| panic!("invalid {opcode} byte PC: {instruction}"))
}

fn assert_machine_navier_artifact(artifacts: &JitArtifactBatch) {
    let bundle = machine_navier_bundle(artifacts);
    let optimized_ir = std::str::from_utf8(
        bundle
            .file(JitArtifactFileName::OptimizedIr)
            .expect("guarded Navier optimized IR")
            .contents(),
    )
    .expect("UTF-8 guarded Navier optimized IR");
    for opcode in ["DecodeNumber", "FloatDiv", "FloatGreaterThan", "BoxNumber"] {
        assert!(
            optimized_ir.contains(opcode),
            "guarded Navier Machine IR must contain {opcode}: {optimized_ir}"
        );
    }
    assert!(
        !optimized_ir.contains("ColdValueExit"),
        "empty feedback must use numeric guards, not a cold-value exit: {optimized_ir}"
    );
    assert_decomposed_element_flow(optimized_ir, 3, 2);

    let code_map = artifact_json(bundle, JitArtifactFileName::CodeMap);
    let regions = code_map["regions"]
        .as_array()
        .expect("guarded Navier code-map regions");
    for kind in [
        "machineElementView",
        "machineElementAddress",
        "machineElementValueLoad",
        "machineElementValueGuard",
        "machineElementValueStore",
    ] {
        let matching = regions
            .iter()
            .filter(|region| region["kind"] == kind)
            .collect::<Vec<_>>();
        assert!(
            !matching.is_empty()
                && matching
                    .iter()
                    .all(|region| region["bytePc"].as_u64().is_some()),
            "guarded Navier must expose bytecode-attributed {kind}: {code_map}"
        );
    }

    let comparison_byte_pc = instruction_byte_pc(bundle, "GreaterThan");
    let division_byte_pc = instruction_byte_pc(bundle, "Div");
    let deopt = artifact_json(bundle, JitArtifactFileName::Deopt);
    let exits = deopt["exits"]
        .as_array()
        .expect("guarded Navier deopt exits");
    let frame_states = deopt["frameStates"]
        .as_array()
        .expect("guarded Navier frame states");
    for (operation, byte_pc) in [
        ("empty-feedback comparison", comparison_byte_pc),
        ("empty-feedback division", division_byte_pc),
    ] {
        assert!(
            exits.iter().any(|exit| {
                let frame_state_id = exit["frameStateId"].as_u64();
                frame_states.iter().any(|state| {
                    state["id"].as_u64() == frame_state_id
                        && state["frames"].as_array().is_some_and(|frames| {
                            frames
                                .iter()
                                .any(|frame| frame["bytePc"].as_u64() == Some(byte_pc))
                        })
                })
            }),
            "{operation} must retain its exact pre-operation frame at bytePc={byte_pc}: {deopt}"
        );
    }
}

fn assert_packed_navier_artifact(artifacts: &JitArtifactBatch) {
    let bundle = packed_navier_bundle(artifacts);
    let optimized_ir = std::str::from_utf8(
        bundle
            .file(JitArtifactFileName::OptimizedIr)
            .expect("packed Navier optimized IR")
            .contents(),
    )
    .expect("UTF-8 packed Navier optimized IR");
    assert_decomposed_element_flow(optimized_ir, 3, 1);

    let code_map = artifact_json(bundle, JitArtifactFileName::CodeMap);
    let regions = code_map["regions"]
        .as_array()
        .expect("packed Navier code-map regions");
    for (kind, expected_count) in [
        ("machineElementView", 4),
        ("machineElementAddress", 4),
        ("machineElementValueLoad", 3),
        ("machineElementValueGuard", 1),
        ("machineElementValueStore", 1),
    ] {
        let matching = regions
            .iter()
            .filter(|region| region["kind"] == kind)
            .collect::<Vec<_>>();
        assert_eq!(
            matching.len(),
            expected_count,
            "packed Navier must expose every {kind}: {code_map}"
        );
        assert!(
            matching
                .iter()
                .all(|region| region["bytePc"].as_u64().is_some()),
            "packed Navier must bytecode-attribute every {kind}: {code_map}"
        );
    }
}

fn assert_machine_number_index_artifact(artifacts: &JitArtifactBatch) {
    let bundle = machine_number_index_bundle(artifacts);
    let optimized_ir = std::str::from_utf8(
        bundle
            .file(JitArtifactFileName::OptimizedIr)
            .expect("Number-index optimized IR")
            .contents(),
    )
    .expect("UTF-8 Number-index optimized IR");
    for opcode in [
        "DecodeNumber",
        "FloatMul",
        "ElementView",
        "ElementAddress",
        "ElementValueLoad",
        "ElementValueGuard",
        "ElementValueStore",
        "BoxNumber",
    ] {
        assert!(
            optimized_ir.contains(opcode),
            "Number-index Machine IR must contain {opcode}: {optimized_ir}"
        );
    }
    assert!(
        !optimized_ir.contains("IntegerMul") && !optimized_ir.contains("ColdValueExit"),
        "the unseen multiplication must remain guarded Float64 work: {optimized_ir}"
    );

    assert_decomposed_element_flow(optimized_ir, 2, 2);

    let code_map = artifact_json(bundle, JitArtifactFileName::CodeMap);
    let regions = code_map["regions"]
        .as_array()
        .expect("Number-index code-map regions");
    for (kind, expected) in [
        ("machineElementValueLoad", 2),
        ("machineElementValueStore", 2),
        ("machineCommittedValueEffect", 4),
    ] {
        assert_eq!(
            regions
                .iter()
                .filter(|region| region["kind"] == kind)
                .count(),
            expected,
            "Number-index body must expose {expected} {kind} regions: {code_map}"
        );
    }
}

fn assert_machine_tagged_index_artifact(artifacts: &JitArtifactBatch) {
    let bundle = machine_tagged_index_bundle(artifacts);
    let optimized_ir = std::str::from_utf8(
        bundle
            .file(JitArtifactFileName::OptimizedIr)
            .expect("tagged-index optimized IR")
            .contents(),
    )
    .expect("UTF-8 tagged-index optimized IR");
    assert_decomposed_element_flow(optimized_ir, 1, 1);
    let load_byte_pc = element_byte_pcs(optimized_ir, "ElementValueLoad")[0];
    let store_byte_pc = element_byte_pcs(optimized_ir, "ElementValueStore")[0];

    let code_map = artifact_json(bundle, JitArtifactFileName::CodeMap);
    let regions = code_map["regions"]
        .as_array()
        .expect("tagged-index code-map regions");
    for (kind, byte_pc) in [
        ("machineElementValueLoad", load_byte_pc),
        ("machineElementValueStore", store_byte_pc),
    ] {
        assert!(
            regions.iter().any(|region| {
                region["kind"] == kind && region["bytePc"].as_u64() == Some(byte_pc)
            }),
            "Tagged-index {kind} must retain bytePc={byte_pc}: {code_map}"
        );
    }
    assert!(
        regions.iter().all(|region| {
            region["kind"] != "machineElementLoad" && region["kind"] != "machineElementStore"
        }),
        "Tagged-index Machine code must not retain generic element regions: {code_map}"
    );
}

fn prepared_navier(selection: JitSelection, artifacts: bool) -> Runtime {
    let mut runtime = runtime(selection, artifacts);
    let setup = runtime
        .run_script(SourceInput::from_javascript(NAVIER_SETUP), NAVIER_MODULE)
        .expect("guarded Navier setup");
    if artifacts {
        assert_machine_navier_artifact(
            setup
                .jit_artifacts()
                .expect("enabled guarded Navier artifact batch"),
        );
    }
    drop(setup);
    runtime
}

fn prepared_packed_navier(selection: JitSelection, artifacts: bool) -> Runtime {
    let mut runtime = runtime(selection, artifacts);
    let setup = runtime
        .run_script(
            SourceInput::from_javascript(PACKED_NAVIER_SETUP),
            PACKED_NAVIER_MODULE,
        )
        .expect("packed Navier setup");
    if artifacts {
        assert_packed_navier_artifact(
            setup
                .jit_artifacts()
                .expect("enabled packed Navier artifact batch"),
        );
    }
    drop(setup);
    runtime
}

fn prepared_number_index(selection: JitSelection, artifacts: bool) -> Runtime {
    let mut runtime = runtime(selection, artifacts);
    let setup = runtime
        .run_script(
            SourceInput::from_javascript(NUMBER_INDEX_SETUP),
            NUMBER_INDEX_MODULE,
        )
        .expect("guarded Number-index setup");
    if artifacts {
        assert_machine_number_index_artifact(
            setup
                .jit_artifacts()
                .expect("enabled guarded Number-index artifact batch"),
        );
    }
    drop(setup);
    runtime
}

fn prepared_tagged_index(selection: JitSelection, artifacts: bool) -> Runtime {
    let mut runtime = runtime(selection, artifacts);
    let setup = runtime
        .run_script(
            SourceInput::from_javascript(TAGGED_INDEX_SETUP),
            TAGGED_INDEX_MODULE,
        )
        .expect("guarded Tagged-index setup");
    if artifacts {
        assert_machine_tagged_index_artifact(
            setup
                .jit_artifacts()
                .expect("enabled guarded Tagged-index artifact batch"),
        );
    }
    drop(setup);
    runtime
}

fn run_with_delta(runtime: &mut Runtime, source: &str, module: &str) -> (String, CounterDelta) {
    let before = runtime.execution_stats();
    let result = completion(runtime, source, module);
    let delta = CounterDelta::between(before, runtime.execution_stats());
    (result, delta)
}

fn assert_delta_constructor_compiles_with_ordered_fields(artifacts: &JitArtifactBatch) {
    let entry_bundle = artifacts
        .bundles()
        .iter()
        .find(|bundle| {
            let manifest = bundle.manifest();
            manifest.module() == DELTA_MODULE
                && manifest.function_name() == "DeltaGuarded"
                && manifest.tier() == JitDebugTier::Optimizing
                && manifest.entry() == JitDebugTarget::Entry
        })
        .expect("DeltaGuarded Optimizing entry artifact");
    let bytecode = std::str::from_utf8(
        entry_bundle
            .file(JitArtifactFileName::Bytecode)
            .expect("DeltaGuarded bytecode artifact")
            .contents(),
    )
    .expect("UTF-8 DeltaGuarded bytecode artifact");
    assert_eq!(
        bytecode
            .lines()
            .filter(|line| line.contains(" StoreProperty "))
            .count(),
        3,
        "DeltaGuarded must retain its three ordered constructor fields: {bytecode}"
    );
}

#[test]
fn packed_navier_uses_decomposed_element_effects() {
    let mut oracle = prepared_packed_navier(JitSelection::InterpreterOnly, false);
    let expected = completion(
        &mut oracle,
        PACKED_NAVIER_PROBE,
        "jit-machine-packed-double-navier-oracle.js",
    );
    assert_eq!(expected, "[0,1.125,1.875,2.625,0]");

    let mut compiled = prepared_packed_navier(JitSelection::ProductionTiered, true);
    let (actual, delta) = run_with_delta(
        &mut compiled,
        PACKED_NAVIER_PROBE,
        "jit-machine-packed-double-navier-final.js",
    );
    assert_eq!(actual, expected);
    assert!(
        delta.optimized_entries > 0,
        "packed Navier probe must enter Machine code: {delta:?}"
    );
    assert_eq!(
        delta.optimized_deopts, 0,
        "packed numeric receivers and values must remain generated: {delta:?}"
    );
}

#[test]
fn navier_mixed_phi_and_empty_feedback_numeric_sites_stay_machine_generated() {
    let mut oracle = prepared_navier(JitSelection::InterpreterOnly, false);
    let expected = completion(
        &mut oracle,
        NAVIER_NUMERIC_PROBE,
        "jit-machine-guarded-numeric-navier-oracle.js",
    );
    assert_eq!(expected, "[4,1]");

    let mut compiled = prepared_navier(JitSelection::ProductionTiered, true);
    let (actual, delta) = run_with_delta(
        &mut compiled,
        NAVIER_NUMERIC_PROBE,
        "jit-machine-guarded-numeric-navier-numeric.js",
    );
    assert_eq!(actual, expected);
    assert!(
        delta.optimized_entries > 0,
        "numeric probe must enter the guarded Navier Machine body: {delta:?}"
    );
    assert_eq!(
        delta.optimized_deopts, 0,
        "supported Number inputs must stay generated: {delta:?}"
    );
}

#[test]
fn guarded_object_and_symbol_exits_coerce_or_throw_once_without_replay() {
    let mut compiled = prepared_navier(JitSelection::ProductionTiered, false);

    let (object_result, object_delta) = run_with_delta(
        &mut compiled,
        NAVIER_OBJECT_PROBE,
        "jit-machine-guarded-numeric-navier-object.js",
    );
    assert_eq!(object_result, "[4,1,1]");
    assert!(
        object_delta.optimized_entries > 0,
        "coercive comparison must first enter Machine code: {object_delta:?}"
    );
    assert_eq!(
        object_delta.optimized_deopts, 1,
        "the comparison object must exact-deopt once before one coercion: {object_delta:?}"
    );

    let (symbol_result, symbol_delta) = run_with_delta(
        &mut compiled,
        NAVIER_SYMBOL_PROBE,
        "jit-machine-guarded-numeric-navier-symbol.js",
    );
    assert_eq!(symbol_result, r#"["TypeError",1]"#);
    assert!(
        symbol_delta.optimized_entries > 0,
        "throwing division must first enter Machine code: {symbol_delta:?}"
    );
    assert_eq!(
        symbol_delta.optimized_deopts, 1,
        "the Symbol divisor must exact-deopt once before the canonical throw: {symbol_delta:?}"
    );

    let (reuse_result, reuse_delta) = run_with_delta(
        &mut compiled,
        NAVIER_REUSE_PROBE,
        "jit-machine-guarded-numeric-navier-reuse.js",
    );
    assert_eq!(reuse_result, "[2,1]");
    assert!(
        reuse_delta.optimized_entries > 0,
        "the guarded Navier generation must remain reusable: {reuse_delta:?}"
    );
    assert_eq!(
        reuse_delta.optimized_deopts, 0,
        "supported reuse must return to the numeric fast path: {reuse_delta:?}"
    );
}

#[test]
fn guarded_number_element_indices_use_direct_or_committed_cold_paths() {
    let mut oracle = prepared_number_index(JitSelection::InterpreterOnly, false);
    let expected_integral = completion(
        &mut oracle,
        NUMBER_INDEX_INTEGRAL_PROBE,
        "jit-machine-guarded-number-index-integral-oracle.js",
    );
    let expected_fractional = completion(
        &mut oracle,
        NUMBER_INDEX_FRACTIONAL_PROBE,
        "jit-machine-guarded-number-index-fractional-oracle.js",
    );
    let expected_negative_zero = completion(
        &mut oracle,
        NUMBER_INDEX_NEGATIVE_ZERO_PROBE,
        "jit-machine-guarded-number-index-negative-zero-oracle.js",
    );
    let expected_reuse = completion(
        &mut oracle,
        NUMBER_INDEX_REUSE_PROBE,
        "jit-machine-guarded-number-index-reuse-oracle.js",
    );
    assert_eq!(expected_integral, "[30,31,1]");
    assert_eq!(expected_fractional, "[null,true,true,1]");
    assert_eq!(expected_negative_zero, "[10,11,1]");
    assert_eq!(expected_reuse, "[40,41,1]");

    let mut compiled = prepared_number_index(JitSelection::ProductionTiered, true);
    let (integral, integral_delta) = run_with_delta(
        &mut compiled,
        NUMBER_INDEX_INTEGRAL_PROBE,
        "jit-machine-guarded-number-index-integral.js",
    );
    assert_eq!(integral, expected_integral);
    assert!(
        integral_delta.optimized_entries > 0,
        "integral Number index must enter Machine code: {integral_delta:?}"
    );
    assert_eq!(
        integral_delta.optimized_deopts, 0,
        "integral Number index must stay on the direct checked path: {integral_delta:?}"
    );

    let (fractional, fractional_delta) = run_with_delta(
        &mut compiled,
        NUMBER_INDEX_FRACTIONAL_PROBE,
        "jit-machine-guarded-number-index-fractional.js",
    );
    assert_eq!(fractional, expected_fractional);
    assert!(
        fractional_delta.optimized_entries > 0,
        "fractional Number index must first enter Machine code: {fractional_delta:?}"
    );
    assert_eq!(
        fractional_delta.optimized_deopts, 1,
        "the element operation must commit once before the later numeric result guard exits: \
         {fractional_delta:?}"
    );

    let (negative_zero, negative_zero_delta) = run_with_delta(
        &mut compiled,
        NUMBER_INDEX_NEGATIVE_ZERO_PROBE,
        "jit-machine-guarded-number-index-negative-zero.js",
    );
    assert_eq!(negative_zero, expected_negative_zero);
    assert!(
        negative_zero_delta.optimized_entries > 0,
        "negative-zero index must first enter Machine code: {negative_zero_delta:?}"
    );
    assert_eq!(
        negative_zero_delta.optimized_deopts, 0,
        "negative zero must remain generated while selecting property key 0: {negative_zero_delta:?}"
    );

    let (reuse, reuse_delta) = run_with_delta(
        &mut compiled,
        NUMBER_INDEX_REUSE_PROBE,
        "jit-machine-guarded-number-index-reuse.js",
    );
    assert_eq!(reuse, expected_reuse);
    assert!(
        reuse_delta.optimized_entries > 0,
        "the Number-index Machine generation must remain reusable: {reuse_delta:?}"
    );
    assert_eq!(
        reuse_delta.optimized_deopts, 0,
        "integral reuse must return to the generated index path: {reuse_delta:?}"
    );
}

#[test]
fn tagged_packed_double_indices_use_direct_or_committed_cold_paths() {
    let mut oracle = prepared_tagged_index(JitSelection::InterpreterOnly, false);
    let expected_integral = completion(
        &mut oracle,
        TAGGED_INDEX_INTEGRAL_PROBE,
        "jit-machine-guarded-tagged-index-integral-oracle.js",
    );
    let expected_string = completion(
        &mut oracle,
        TAGGED_INDEX_STRING_PROBE,
        "jit-machine-guarded-tagged-index-string-oracle.js",
    );
    let expected_fractional = completion(
        &mut oracle,
        TAGGED_INDEX_FRACTIONAL_PROBE,
        "jit-machine-guarded-tagged-index-fractional-oracle.js",
    );
    let expected_reuse = completion(
        &mut oracle,
        TAGGED_INDEX_REUSE_PROBE,
        "jit-machine-guarded-tagged-index-reuse-oracle.js",
    );
    assert_eq!(expected_integral, "[30,30.5,1]");
    assert_eq!(expected_string, "[20,20.5,1]");
    assert_eq!(expected_fractional, "[null,true,true,1]");
    assert_eq!(expected_reuse, "[40,40.5,1]");

    let mut compiled = prepared_tagged_index(JitSelection::ProductionTiered, true);
    let (integral, integral_delta) = run_with_delta(
        &mut compiled,
        TAGGED_INDEX_INTEGRAL_PROBE,
        "jit-machine-guarded-tagged-index-integral.js",
    );
    assert_eq!(integral, expected_integral);
    assert!(
        integral_delta.optimized_entries > 0,
        "a tagged int32 index must enter Machine code: {integral_delta:?}"
    );
    assert_eq!(
        integral_delta.optimized_deopts, 0,
        "a tagged int32 index must remain on the packed path: {integral_delta:?}"
    );

    let (string, string_delta) = run_with_delta(
        &mut compiled,
        TAGGED_INDEX_STRING_PROBE,
        "jit-machine-guarded-tagged-index-string.js",
    );
    assert_eq!(string, expected_string);
    assert!(
        string_delta.optimized_entries > 0,
        "a string index must first enter Machine code: {string_delta:?}"
    );
    assert_eq!(
        string_delta.optimized_deopts, 0,
        "a string index must use the committed cold sibling without deopt: {string_delta:?}"
    );

    let (fractional, fractional_delta) = run_with_delta(
        &mut compiled,
        TAGGED_INDEX_FRACTIONAL_PROBE,
        "jit-machine-guarded-tagged-index-fractional.js",
    );
    assert_eq!(fractional, expected_fractional);
    assert!(
        fractional_delta.optimized_entries > 0,
        "a fractional tagged index must first enter Machine code: {fractional_delta:?}"
    );
    assert_eq!(
        fractional_delta.optimized_deopts, 1,
        "the element operation must commit once before the later numeric result guard exits: \
         {fractional_delta:?}"
    );

    let (reuse, reuse_delta) = run_with_delta(
        &mut compiled,
        TAGGED_INDEX_REUSE_PROBE,
        "jit-machine-guarded-tagged-index-reuse.js",
    );
    assert_eq!(reuse, expected_reuse);
    assert!(
        reuse_delta.optimized_entries > 0,
        "the tagged-index Machine generation must remain reusable: {reuse_delta:?}"
    );
    assert_eq!(
        reuse_delta.optimized_deopts, 0,
        "fresh tagged int32 reuse must return to the packed path: {reuse_delta:?}"
    );
}

#[test]
fn delta_mixed_default_constructor_compiles_and_preserves_fields() {
    let mut oracle = runtime(JitSelection::InterpreterOnly, false);
    completion(
        &mut oracle,
        DELTA_SETUP,
        "jit-delta-guarded-oracle-setup.js",
    );
    let expected = completion(
        &mut oracle,
        DELTA_PROBE,
        "jit-delta-guarded-oracle-probe.js",
    );
    assert_eq!(
        expected,
        r#"[[0,0,"delta","value,next,marker"],[7,7,"delta","value,next,marker"],["4","4","delta","value,next,marker"]]"#
    );

    let mut compiled = runtime(JitSelection::ProductionTiered, true);
    let setup = compiled
        .run_script(SourceInput::from_javascript(DELTA_SETUP), DELTA_MODULE)
        .expect("DeltaGuarded setup");
    assert_delta_constructor_compiles_with_ordered_fields(
        setup
            .jit_artifacts()
            .expect("enabled DeltaGuarded artifact batch"),
    );
    drop(setup);

    let (actual, delta) = run_with_delta(
        &mut compiled,
        DELTA_PROBE,
        "jit-machine-guarded-numeric-delta-probe.js",
    );
    assert_eq!(actual, expected);
    assert!(
        delta.optimized_entries > 0,
        "the mixed-default DeltaGuarded constructor must enter Machine code: {delta:?}"
    );
}
