//! Normal-policy proof of complete shape lookup and its exact recovery edge.
//!
//! # Contents
//! - Megamorphic own reads enter an installed Graph generation, then an
//!   accessor mutation retrains each source get without replay.
//!
//! # Invariants
//! - Artifact capture alone does not prove execution: isolated entry probes
//!   must retain the exact generation without interpreter/Template dispatch.
//! - Every accessor read executes once across the typed miss and recovery.
//!
//! # See also
//! - `crate::compare_with_entry_probes` owns the common execution proof.
use super::*;

#[test]
fn complete_shape_directory_enters_native_code_and_recovers_each_get_for_accessors() {
    const SETUP: &str = r#"
const directoryObjects = [];
for (let index = 0; index < 18; index++) {
  const record = {};
  for (let key = 0; key < index; key++) record['before' + key] = key;
  record.value = index + 1;
  for (let key = 0; key < 12; key++) record['after' + key] = key;
  directoryObjects.push(record);
}
function directoryRead(record) {
  for (let spin = 0; spin < 3; spin++) {}
  const first = record.value;
  return first + record.value;
}
const directoryArgs = [directoryObjects[0]];
for (let call = 0; call < 9000; call++) {
  directoryArgs[0] = directoryObjects[call % 18];
  Reflect.apply(directoryRead, undefined, directoryArgs);
}
"#;
    const MUTATE: &str = r#"
let directoryGets = 0;
Object.defineProperty(directoryObjects[0], 'value', {
  get() { directoryGets++; return 11; }, configurable: true
});
directoryArgs[0] = directoryObjects[0];
let directorySum = 0;
for (let call = 0; call < 5000; call++) directorySum += Reflect.apply(directoryRead, undefined, directoryArgs);
String(directorySum) + '|' + directoryGets;
"#;
    let compiled = compare_with_entry_probes(
        SETUP,
        MUTATE,
        "110000|10000",
        &[entry_probe(
            "directoryRead",
            "String(directoryRead(directoryObjects[7]));",
            "16",
        )],
        &[entry_probe(
            "directoryRead",
            "String(directoryRead(directoryObjects[7]));",
            "16",
        )],
    );
    let generation = compiled
        .entry_probes
        .iter()
        .find(|(name, _)| name == "directoryRead")
        .expect("native directory generation")
        .1;
    let ir = &compiled.optimized_ir[&("directoryRead".to_owned(), generation)];
    assert_eq!(
        ir.matches("LoadShapedProperty").count(),
        1,
        "repeated data read must be commoned: {ir}"
    );
    let exits = compiled.optimizing_exits("directoryRead");
    let misses = exits
        .iter()
        .filter(|(_, reason)| *reason == ExitReason::OwnDataMiss)
        .count();
    assert_eq!(
        misses, 2,
        "one miss per original get after the first becomes effectful: {exits:?}"
    );
    compiled.assert_recompiled_after_last_exit("directoryRead");
}
