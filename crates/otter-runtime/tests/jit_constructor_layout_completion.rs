//! Mixed-super completion timing and retained receiver footprint proofs.
//!
//! # Contents
//! - Native/Proxy first super followed by a rejected bytecode second super.
//! - Successful transparent Proxy/Bound Super and observable-trap ownership.
//! - Escaped rejected receivers grow after child terminal sampling.
//! - First-seven retained cells and the future finalized cell survive full GC.
//!
//! # Invariants
//! Scoped observers record owned cell sizes without JS allocation or reentry. The scoped
//! motion probe retains handles throughout actual collection. Arguments are
//! rooted by the production native call boundary; no receiver, raw address or
//! VM handle escapes into a Rust record. Callback premise failures are asserted
//! outside the ABI; no assertion panics across a native entry.
//!
//! # See also
//! - `otter_vm::constructor_layout::completion` owns terminal ticket sampling.
//! - `jit_stack_owned_array_construct` owns strict generated receiver LAB fits.

use otter_runtime::{
    JitSelection, Runtime, RuntimeExtensionInstaller, RuntimeNativeCall, RuntimeNativeCtx,
    RuntimeNativeError, RuntimeValue, SourceInput,
};
use std::sync::{Arc, Mutex};

#[test]
fn mixed_native_or_proxy_super_samples_rejected_receiver_before_escaped_mutation() {
    for (lexical_label, bind_source) in [
        ("own", "super()"),
        ("arrow", "(() => super())()"),
        ("eval", "eval(\"super()\")"),
    ] {
        for proxy in [false, true] {
            for selection in [
                JitSelection::InterpreterOnly,
                JitSelection::Template,
                JitSelection::ProductionTiered,
            ] {
                let records = Arc::new(Mutex::new(Vec::<Result<usize, String>>::new()));
                let installer =
                    {
                        let records = records.clone();
                        RuntimeExtensionInstaller::new(move |realm| {
                            let records = records.clone();
                            realm.install_native_global_call(
                            "constructorCellSize",
                            1,
                            RuntimeNativeCall::Dynamic(Arc::new(
                                move |ctx: &mut RuntimeNativeCtx<'_>,
                                      args: &[RuntimeValue],
                                      _state: &[RuntimeValue]| {
                                    let observation = args
                                        .first()
                                        .and_then(|value| value.as_object())
                                        .and_then(|object| {
                                        ctx.scope(|mut scope| {
                                            let object = scope.value(RuntimeValue::object(object));
                                            scope.object_allocation_bytes(object)
                                        }).ok()
                                    })
                                        .ok_or_else(|| {
                                            "observer expected ordinary receiver".to_owned()
                                        });
                                    records
                                        .lock()
                                        .map_err(|_| RuntimeNativeError::Error {
                                            message: "constructor observation lock poisoned".into(),
                                        })?
                                        .push(observation);
                                    Ok(RuntimeValue::undefined())
                                },
                            )),
                        )
                        })
                    };
                let native = if proxy {
                    "new Proxy(Array, {})"
                } else {
                    "Array"
                };
                let source = format!(
                    r#"
const firstNativeSuper = {native};
const escapedRejectedReceivers = [];
function SecondBytecodeBase() {{
  this.only = 1;
  constructorCellSize(this);
  escapedRejectedReceivers.push(this);
}}
class MixedSuper extends firstNativeSuper {{
  constructor() {{
    Object.setPrototypeOf(MixedSuper, firstNativeSuper);
    {bind_source};
    Object.setPrototypeOf(MixedSuper, SecondBytecodeBase);
    let rejected = false;
    try {{ {bind_source}; }} catch (error) {{ rejected = error instanceof ReferenceError; }}
    if (!rejected) throw "second super must reject already bound this";
    const orphan = escapedRejectedReceivers[escapedRejectedReceivers.length - 1];
    // These mutations occur after the child's successful completion and the
    // actual BindThis rejection, but before the outer constructor terminal.
    orphan.after0 = 0; orphan.after1 = 1; orphan.after2 = 2; orphan.after3 = 3;
    orphan.after4 = 4; orphan.after5 = 5; orphan.after6 = 6; orphan.after7 = 7;
    if (!Array.isArray(this)) throw "first native receiver must remain this";
  }}
}}
const mixedOuterReceivers = [];
for (let index = 0; index < 8; index++) mixedOuterReceivers.push(new MixedSuper());
JSON.stringify([mixedOuterReceivers.length, escapedRejectedReceivers.length,
  mixedOuterReceivers.every(value => Array.isArray(value)),
  escapedRejectedReceivers.every(value => value.only === 1 && value.after7 === 7),
  escapedRejectedReceivers.every(value => Object.getPrototypeOf(value) === MixedSuper.prototype)]);
"#
                );
                let mut runtime = Runtime::builder()
                    .jit_selection(selection)
                    .extension_installer(installer)
                    .build()
                    .expect("mixed-super runtime");
                let result = runtime
                    .run_script(
                        SourceInput::from_javascript(source),
                        "mixed-constructor-terminal.js",
                    )
                    .expect("mixed-super completion");
                assert_eq!(
                    result.completion_string(),
                    "[8,8,true,true,true]",
                    "{selection:?}, proxy={proxy}, lexical={lexical_label}"
                );
                runtime
                    .force_gc()
                    .expect("retain actual family heads and all first-seven cells");
                runtime.run_script(SourceInput::from_javascript(
                "for (let index = 0; index < 8; index++) constructorCellSize(escapedRejectedReceivers[index]);"),
                "mixed-constructor-retained-cells.js").expect("retained cells probe");
                let observed = records.lock().unwrap();
                let sizes = observed
                    .iter()
                    .map(|row| row.as_ref().copied())
                    .collect::<Result<Vec<_>, _>>()
                    .expect("owned observation premises");
                let prefix = otter_gc::header::HEADER_SIZE
                    + std::mem::size_of::<otter_vm::object::ObjectBody>();
                let mut expected = vec![prefix + 64 * 8; 7];
                expected.push(prefix + 8);
                expected.extend_from_within(..);
                assert_eq!(
                    sizes, expected,
                    "{selection:?}, proxy={proxy}, lexical={lexical_label}: rejected child samples at one-field terminal; later escaped growth cannot enlarge finalized future cells or shrink first seven"
                );
            }
        }
    }
}

#[test]
fn arrow_and_eval_super_keep_ticket_until_the_actual_outer_construct_terminal() {
    for (label, bind_source) in [
        ("arrow", "(() => super())()"),
        ("eval", "eval(\"super()\")"),
    ] {
        for selection in [
            JitSelection::InterpreterOnly,
            JitSelection::Template,
            JitSelection::ProductionTiered,
        ] {
            let records = Arc::new(Mutex::new(Vec::<Result<usize, String>>::new()));
            let installer = {
                let records = records.clone();
                RuntimeExtensionInstaller::new(move |realm| {
                    let records = records.clone();
                    realm.install_native_global_call(
                        "lexicalCellSize",
                        1,
                        RuntimeNativeCall::Dynamic(Arc::new(
                            move |ctx: &mut RuntimeNativeCtx<'_>,
                                  args: &[RuntimeValue],
                                  _state: &[RuntimeValue]| {
                                let observation = args
                                    .first()
                                    .and_then(|value| value.as_object())
                                    .and_then(|object| {
                                        ctx.scope(|mut scope| {
                                            let object = scope.value(RuntimeValue::object(object));
                                            scope.object_allocation_bytes(object)
                                        })
                                        .ok()
                                    })
                                    .ok_or_else(|| "expected actual base receiver".to_owned());
                                records
                                    .lock()
                                    .map_err(|_| RuntimeNativeError::Error {
                                        message: "lexical observer lock poisoned".into(),
                                    })?
                                    .push(observation);
                                Ok(RuntimeValue::undefined())
                            },
                        )),
                    )
                })
            };
            let source = format!(
                r#"
const lexicalRetainedReceivers = [];
function LexicalLayoutBase() {{ this.base = 1; lexicalCellSize(this); }}
class LexicalLayoutDerived extends LexicalLayoutBase {{
  constructor() {{
    {bind_source};
    // Dynamic own names cannot become a static constructor slot minimum.
    // All eight fields are added AFTER the child super terminal and lexical
    // context bind. Only the actual outer terminal may sample them.
    for (let index = 0; index < 8; index++) this["outer" + index] = index;
    lexicalRetainedReceivers.push(this);
  }}
}}
for (let index = 0; index < 8; index++) new LexicalLayoutDerived();
JSON.stringify([lexicalRetainedReceivers.length,
  lexicalRetainedReceivers.every(value => value.base === 1 && value.outer7 === 7),
  lexicalRetainedReceivers.every(value => Object.getPrototypeOf(value) === LexicalLayoutDerived.prototype)]);
"#
            );
            let mut runtime = Runtime::builder()
                .jit_selection(selection)
                .extension_installer(installer)
                .build()
                .expect("lexical-super runtime");
            let result = runtime
                .run_script(
                    SourceInput::from_javascript(source),
                    "lexical-constructor-terminal.js",
                )
                .expect("lexical-super actual completion");
            assert_eq!(
                result.completion_string(),
                "[8,true,true]",
                "{label}, {selection:?}"
            );
            runtime
                .force_gc()
                .expect("retained cells and exact family survive full GC");
            runtime.run_script(SourceInput::from_javascript(
                "for (let index = 0; index < 8; index++) lexicalCellSize(lexicalRetainedReceivers[index]);"),
                "lexical-constructor-retained.js").expect("retained lexical cells");
            let rows = records.lock().unwrap();
            let sizes = rows
                .iter()
                .map(|row| row.as_ref().copied())
                .collect::<Result<Vec<_>, _>>()
                .expect("owned lexical callback premises");
            let prefix =
                otter_gc::header::HEADER_SIZE + std::mem::size_of::<otter_vm::object::ObjectBody>();
            let mut expected = vec![prefix + 64 * 8; 7];
            expected.push(prefix + 9 * 8);
            expected.extend_from_within(..);
            assert_eq!(
                sizes, expected,
                "{label}, {selection:?}: context-only super transfers to exact outer terminal; future capacity includes all nine dynamic observed fields"
            );
        }
    }
}

#[path = "support/moving_children.rs"]
mod moving_children;

#[path = "jit_constructor_layout_completion/forward.rs"]
mod forward;
