//! Successful transparent Super forwarding with exact terminal geometry.
//!
//! # Contents
//! - Default Proxy and bound bytecode targets from own, arrow and eval sources.
//! - Observable user-trap ordinary construction as the origin-clearing control.
//! - Real scoped evacuation inside the base and retained full-GC validation.
//!
//! # Invariants
//! - A successful transparent edge samples after all dynamic outer writes.
//! - User-trap nested new owns its terminal sample before outer writes.
//! - Moving observations retain handles during GC and only owned scalars after.
//! - Callback failures are recorded or returned; assertions never unwind an ABI.
//! - Every tier uses normal policy; this fixture claims semantic transport,
//!   not native admission inferred from a short call count.
//!
//! # See also
//! - `super::moving_children` owns the existing handle-scoped collection probe.
//! - `jit_stack_owned_array_construct::first_seven` proves current native reuse.

use super::moving_children::ChildMotion;
use super::*;

#[derive(Clone, Copy, Debug)]
enum Route {
    TransparentProxy,
    Bound,
    UserTrap,
}

impl Route {
    fn expression(self) -> &'static str {
        match self {
            Self::TransparentProxy => "new Proxy(ForwardBytecodeBase, {})",
            Self::Bound => "ForwardBytecodeBase.bind(null, 17)",
            Self::UserTrap => {
                r#"new Proxy(ForwardBytecodeBase, {
  construct(target, args, newTarget) {
    forwardTrapCalls++;
    return Reflect.construct(target, args, newTarget);
  }
})"#
            }
        }
    }

    fn call(self) -> &'static str {
        match self {
            Self::Bound => "super(value)",
            Self::TransparentProxy | Self::UserTrap => "super(17, value)",
        }
    }

    /// Every route's lineage tree holds the base's four fields and the
    /// derived constructor's eight, whichever terminal sampled the receiver.
    fn final_capacity(self) -> usize {
        12
    }
}

fn cell_bytes(capacity: usize) -> usize {
    otter_gc::header::HEADER_SIZE
        + std::mem::size_of::<otter_vm::object::ObjectBody>()
        + capacity * std::mem::size_of::<RuntimeValue>()
}

#[test]
fn transparent_proxy_and_bound_super_transfer_successfully_from_exact_lexical_source() {
    for route in [Route::TransparentProxy, Route::Bound, Route::UserTrap] {
        for lexical in ["own", "arrow", "eval"] {
            for selection in [
                JitSelection::InterpreterOnly,
                JitSelection::Template,
                JitSelection::ProductionTiered,
            ] {
                let geometry = Arc::new(Mutex::new(Vec::<Result<usize, String>>::new()));
                let motion = Arc::new(Mutex::new(Vec::<Result<Vec<ChildMotion>, String>>::new()));
                let installer =
                    {
                        let geometry = geometry.clone();
                        let motion = motion.clone();
                        RuntimeExtensionInstaller::new(move |realm| {
                            super::moving_children::install(realm)?;
                            realm.install_native_global_call(
                                "forwardResetStress",
                                0,
                                RuntimeNativeCall::Dynamic(Arc::new(
                                    |ctx: &mut RuntimeNativeCtx<'_>,
                                     _args: &[RuntimeValue],
                                     _state: &[RuntimeValue]| {
                                        // Exact evacuation needs fresh young arguments. This
                                        // fixture controls collection explicitly for every
                                        // base call; it never lowers tier admission policy.
                                        ctx.interp_mut().gc_heap_mut().set_gc_stress(0, false);
                                        Ok(RuntimeValue::undefined())
                                    },
                                )),
                            )?;
                            let motion = motion.clone();
                            realm.install_native_global_call(
                            "forwardMove",
                            3,
                            RuntimeNativeCall::Dynamic(Arc::new(
                                move |ctx: &mut RuntimeNativeCtx<'_>,
                                      args: &[RuntimeValue],
                                      _state: &[RuntimeValue]| {
                                    let observation = if args.len() == 3 {
                                        super::moving_children::observe_and_collect(ctx, args)
                                            .map_err(|error| error.to_string())
                                    } else {
                                        Err("motion requires receiver and two children".to_owned())
                                    };
                                    motion
                                        .lock()
                                        .map_err(|_| RuntimeNativeError::Error {
                                            message: "forward motion recorder poisoned".into(),
                                        })?
                                        .push(observation);
                                    Ok(RuntimeValue::undefined())
                                },
                            )),
                        )?;
                            let geometry = geometry.clone();
                            realm.install_native_global_call(
                            "forwardCellSize",
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
                                            "geometry requires actual receiver".to_owned()
                                        });
                                    geometry
                                        .lock()
                                        .map_err(|_| RuntimeNativeError::Error {
                                            message: "forward geometry recorder poisoned".into(),
                                        })?
                                        .push(observation);
                                    Ok(RuntimeValue::undefined())
                                },
                            )),
                        )
                        })
                    };
                let call = route.call();
                let bind = match lexical {
                    "own" => call.to_owned(),
                    "arrow" => format!("(() => {call})()"),
                    "eval" => format!("eval(\"{call}\")"),
                    _ => unreachable!("fixed lexical source matrix"),
                };
                let target = route.expression();
                let source = format!(
                    r#"
forwardResetStress();
let forwardBaseCalls = 0;
let forwardTrapCalls = 0;
const forwardRetained = [];
function ForwardBytecodeBase(prefix, value) {{
  forwardBaseCalls++;
  this.marker = prefix * 100 + value;
  this.childA = {{ marker: this.marker + 10000 }};
  this.childB = {{ marker: this.marker + 20000 }};
  this.target = new.target;
  // The exact Super source and its lexical binding context are suspended
  // while this callback moves the receiver and both property children.
  forwardMove(this, this.childA, this.childB);
  forwardCellSize(this);
}}
const ForwardTarget = {target};
// A bound constructor has no own prototype until explicitly installed;
// class heritage must still derive from the actual target prototype.
Object.defineProperty(ForwardTarget, "prototype", {{ value: ForwardBytecodeBase.prototype }});
class ForwardDerived extends ForwardTarget {{
  constructor(value) {{
    const actual = {bind};
    if (actual !== this) throw "successful Super result did not bind exact this";
    for (let field = 0; field < 8; field++) this["outer" + field] = value + field;
    forwardRetained.push(this);
  }}
}}
for (let value = 0; value < 8; value++) new ForwardDerived(value);
JSON.stringify([forwardRetained.length, forwardBaseCalls, forwardTrapCalls,
  forwardRetained.every((value, index) => value.marker === 1700 + index && value.outer7 === index + 7),
  forwardRetained.every(value => value.childA.marker === value.marker + 10000 && value.childB.marker === value.marker + 20000),
  forwardRetained.every(value => value.target === ForwardDerived && Object.getPrototypeOf(value) === ForwardDerived.prototype),
  forwardRetained.every(value => value instanceof ForwardDerived && value instanceof ForwardBytecodeBase),
  forwardRetained.every((value, index) => value.childA !== value.childB && forwardRetained.every((other, otherIndex) => index === otherIndex || value !== other))]);
"#
                );
                let mut runtime = Runtime::builder()
                    .jit_selection(selection)
                    .extension_installer(installer)
                    .build()
                    .expect("transparent Super runtime");
                let before = runtime.execution_stats();
                let result = runtime
                    .run_script(
                        SourceInput::from_javascript(source),
                        "transparent-super-source.js",
                    )
                    .expect("successful transparent and user-trap Super semantics");
                let expected_traps = usize::from(matches!(route, Route::UserTrap)) * 8;
                assert_eq!(
                    result.completion_string(),
                    format!("[8,8,{expected_traps},true,true,true,true,true]"),
                    "{selection:?}, {route:?}, lexical={lexical}"
                );
                let after = runtime.execution_stats();
                assert!(after.gc_cycles >= before.gc_cycles + 8);
                let movements = motion.lock().unwrap();
                assert_eq!(
                    movements.len(),
                    8,
                    "one committed base motion callback per Super"
                );
                for (index, row) in movements.iter().enumerate() {
                    let row = row.as_ref().expect("owned motion callback premises");
                    assert_eq!(row.len(), 3);
                    for (position, child) in row.iter().enumerate() {
                        assert_ne!(
                            child.before, child.after,
                            "exact argument {position} moved at Super {index}"
                        );
                        assert_eq!(child.marker_before, child.marker_after);
                        assert_eq!(child.marker_after, (1700 + index + position * 10000) as f64);
                        assert!(
                            !row[..position]
                                .iter()
                                .any(|prior| prior.before == child.before)
                        );
                        assert!(
                            !row[..position]
                                .iter()
                                .any(|prior| prior.after == child.after)
                        );
                    }
                }
                drop(movements);
                runtime
                    .force_gc()
                    .expect("full GC with retained forwarded receivers");
                let retained = runtime.run_script(SourceInput::from_javascript(
                    r#"for (let index = 0; index < 8; index++) forwardCellSize(forwardRetained[index]);
JSON.stringify([forwardBaseCalls, forwardTrapCalls,
  forwardRetained.every((value, index) => value.marker === 1700 + index && value.outer7 === index + 7),
  forwardRetained.every(value => value.childA.marker === value.marker + 10000 && value.childB.marker === value.marker + 20000),
  forwardRetained.every(value => value.target === ForwardDerived && Object.getPrototypeOf(value) === ForwardDerived.prototype)]);"#),
                    "transparent-super-retained.js").expect("retained forwarding semantics");
                assert_eq!(
                    retained.completion_string(),
                    format!("[8,{expected_traps},true,true,true]")
                );
                let rows = geometry.lock().unwrap();
                let sizes = rows
                    .iter()
                    .map(|row| row.as_ref().copied())
                    .collect::<Result<Vec<_>, _>>()
                    .expect("owned geometry callback premises");
                let mut expected = vec![cell_bytes(64); 7];
                expected.push(cell_bytes(route.final_capacity()));
                expected.extend_from_within(..);
                assert_eq!(
                    sizes, expected,
                    "{selection:?}, {route:?}, lexical={lexical}: the finished lineage holds every field its transition tree places; first7 cells never shrink"
                );
            }
        }
    }
}
