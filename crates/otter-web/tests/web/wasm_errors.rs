//! Actual Wasmtime host traps, intrinsic errors and moving exception identity.
//!
//! # Contents
//! - Export and start-function structural/control/OOM propagation.
//! - Catchable class projection and original user-value JSTag round trips.
//! - Collecting externref imports using the existing borrowed Caller.
//!
//! # Invariants
//! - Every transport proof executes a compiled Wasmtime function or start body.
//! - Fatal causes cannot reach either Wasm catch_all or JavaScript catch/reject.
//! - GC callbacks retain scoped values and record scalar results; assertions run
//!   outside the native ABI. Strong-root cleanup precedes consuming the scope.
//! - OOM transport uses a real completed Error-allocation refusal at the cap;
//!   explicitly authored native OOM retains its ordinary catchable domain.
//! - Mutable JS constructor bindings never select engine-produced errors.
//!
//! # See also
//! - `otter_web::wasm` owns the existing Wasmtime Store and typed boundary.

use super::*;
use otter_runtime::marshal::JsError;
use otter_runtime::{
    DiagnosticCode, OtterError, RuntimeExtensionInstaller, RuntimeNativeCall, RuntimeNativeCtx,
    RuntimeNativeError, RuntimeValue,
};
use std::sync::{Arc, Mutex};

fn bytes(wat: &str) -> String {
    wat::parse_str(wat)
        .expect("verified Wasm fixture")
        .iter()
        .map(u8::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

#[derive(Debug, Clone)]
struct Motion {
    before: u32,
    after: u32,
    marker: f64,
    minor_before: u64,
    minor_after: u64,
}
#[derive(Default)]
struct Records {
    motions: Vec<Result<Motion, String>>,
    oom: Option<(u64, u64)>,
}

fn fail(code: f64) -> RuntimeNativeError {
    match code as u32 {
        0 => RuntimeNativeError::InvalidOperand,
        1 => RuntimeNativeError::MissingReturn,
        2 => RuntimeNativeError::Interrupted,
        3 => RuntimeNativeError::BudgetExceeded {
            reason: "original wasm callback budget".into(),
        },
        4 => RuntimeNativeError::Exit { code: 27 },
        _ => RuntimeNativeError::SyntaxError {
            name: "wasm failure",
            reason: "original syntax payload".into(),
        },
    }
}

fn runtime(records: Arc<Mutex<Records>>) -> Runtime {
    Runtime::builder()
        .with_web_apis()
        .max_heap_bytes(8 * 1024 * 1024)
        .extension_installer(RuntimeExtensionInstaller::new(move |realm| {
            realm.install_native_global_call(
                "wasmFailure",
                1,
                RuntimeNativeCall::Dynamic(Arc::new(
                    |_ctx: &mut RuntimeNativeCtx<'_>,
                     args: &[RuntimeValue],
                     _state: &[RuntimeValue]| {
                        Err(fail(args.first().and_then(|v| v.as_f64()).unwrap_or(0.0)))
                    },
                )),
            )?;
            realm.install_native_global_call(
                "wasmReset",
                0,
                RuntimeNativeCall::Dynamic(Arc::new(
                    |ctx: &mut RuntimeNativeCtx<'_>,
                     _args: &[RuntimeValue],
                     _state: &[RuntimeValue]| {
                        ctx.interp_mut().gc_heap_mut().set_gc_stress(0, false);
                        Ok(RuntimeValue::undefined())
                    },
                )),
            )?;
            let motion_records = records.clone();
            realm.install_native_global_call(
                "wasmMove",
                1,
                RuntimeNativeCall::Dynamic(Arc::new(
                    move |ctx: &mut RuntimeNativeCtx<'_>,
                          args: &[RuntimeValue],
                          _state: &[RuntimeValue]| {
                        ctx.scope(|mut scope| {
                            let value = scope.argument(args, 0);
                            let root = scope.persistent_root_insert(value);
                            let ambient = scope.async_context();
                            let moved = scope.with_async_context(ambient, |ctx| {
                                let before = ctx
                                    .persistent_root_get(root)
                                    .and_then(|value| value.as_object())
                                    .ok_or(RuntimeNativeError::InvalidOperand)?
                                    .offset();
                                let stats = ctx.interp_mut().gc_stats_snapshot();
                                ctx.interp_mut().force_gc().map_err(|error| {
                                    JsError::from_vm(ctx.interp_mut(), error)
                                        .into_native("wasmMove")
                                })?;
                                let after = ctx
                                    .persistent_root_get(root)
                                    .and_then(|value| value.as_object())
                                    .ok_or(RuntimeNativeError::InvalidOperand)?
                                    .offset();
                                let after_stats = ctx.interp_mut().gc_stats_snapshot();
                                Ok::<_, RuntimeNativeError>((
                                    before,
                                    after,
                                    stats.minor_gc_cycles,
                                    after_stats.minor_gc_cycles,
                                ))
                            });
                            // Remove the existing strong root on both collector
                            // success and failure. Both Local aliases are still
                            // traced by this live, unfinished outer scope.
                            let retained = scope
                                .take_persistent_root(root)
                                .ok_or(RuntimeNativeError::InvalidOperand)?;
                            if !scope.strict_equals(retained, value) {
                                return Err(RuntimeNativeError::InvalidOperand);
                            }
                            let (before, after, minor_before, minor_after) = moved?;
                            let marker = scope.get(value, "marker")?;
                            let marker = scope.number_value(marker)?;
                            motion_records
                                .lock()
                                .map_err(|_| RuntimeNativeError::InvalidOperand)?
                                .motions
                                .push(Ok(Motion {
                                    before,
                                    after,
                                    marker,
                                    minor_before,
                                    minor_after,
                                }));
                            Ok(scope.finish(value))
                        })
                    },
                )),
            )?;
            let oom_records = records.clone();
            realm.install_native_global_call(
                "wasmAllocate",
                0,
                RuntimeNativeCall::Dynamic(Arc::new(
                    move |ctx: &mut RuntimeNativeCtx<'_>,
                          _args: &[RuntimeValue],
                          _state: &[RuntimeValue]| {
                        // Enter a real completed native call. Building its
                        // oversized SyntaxError fails the actual VM cap; the
                        // existing Host completion imports that exact failure.
                        // A direct scope.string refusal would instead author
                        // a catchable NativeError::OutOfMemory.
                        let result = ctx.scope(|mut scope| {
                            let callback = scope
                                .global("wasmHugeError")
                                .ok_or(RuntimeNativeError::InvalidOperand)?;
                            let receiver = scope.undefined();
                            let value = scope.call(callback, receiver, &[])?;
                            Ok(scope.finish(value))
                        });
                        if let Err(RuntimeNativeError::ExecutionFailure(failure)) = &result
                            && let otter_vm::VmError::OutOfMemory {
                                requested_bytes,
                                heap_limit_bytes,
                            } = failure.error
                        {
                            oom_records
                                .lock()
                                .map_err(|_| RuntimeNativeError::InvalidOperand)?
                                .oom = Some((requested_bytes, heap_limit_bytes));
                        }
                        result
                    },
                )),
            )?;
            realm.install_native_global_call(
                "wasmHugeError",
                0,
                RuntimeNativeCall::Dynamic(Arc::new(
                    |_ctx: &mut RuntimeNativeCtx<'_>,
                     _args: &[RuntimeValue],
                     _state: &[RuntimeValue]| {
                        Err(RuntimeNativeError::SyntaxError {
                            name: "wasmHugeError",
                            reason: "s".repeat(16 * 1024 * 1024),
                        })
                    },
                )),
            )?;
            Ok(())
        }))
        .build()
        .expect("Wasm runtime")
}

const CALL: &str = r#"(module
    (import "env" "callback" (func $callback))
    (func (export "call") (result i32)
      (block $caught (try_table (catch_all $caught) call $callback)
        i32.const 1 return)
      i32.const 2))"#;
const START: &str = r#"(module
    (import "env" "callback" (func $callback))
    (func $start call $callback) (start $start))"#;

#[test]
fn wasm_export_and_start_keep_typed_fatal_control_and_actual_oom() {
    for start in [false, true] {
        for code in 0..=5 {
            let records = Arc::new(Mutex::new(Records::default()));
            let mut runtime = runtime(records.clone());
            let binary = bytes(if start { START } else { CALL });
            let callback = if code == 5 {
                "wasmAllocate()".to_owned()
            } else {
                format!("wasmFailure({code})")
            };
            let invoke = if start {
                "new WebAssembly.Instance(module, imports)"
            } else {
                "new WebAssembly.Instance(module, imports).exports.call()"
            };
            let result = runtime.run_script(
                SourceInput::from_javascript(format!(
                    r#"
                wasmReset(); globalThis.wasmCaught=0; globalThis.wasmReturned=0;
                const module=new WebAssembly.Module(new Uint8Array([{binary}]));
                const imports={{env:{{callback:()=>{callback}}}}};
                try {{ {invoke}; wasmReturned++; }} catch(error) {{ wasmCaught++; }}
                91;
            "#
                )),
                "wasm-fatal.js",
            );
            if code == 4 {
                assert_eq!(
                    result.expect("typed exit completion").exit_code_override(),
                    Some(27)
                );
            } else {
                let error = result.expect_err("fatal host cause must leave actual Wasmtime call");
                match code {
                    0 | 1 => assert!(
                        matches!(error,OtterError::Internal{ref code,..} if code==DiagnosticCode::VmBytecodeInvariant.as_str()),
                        "{error:?}"
                    ),
                    2 => assert!(matches!(error, OtterError::Interrupted), "{error:?}"),
                    3 => assert!(
                        matches!(error,OtterError::Runtime{ref diagnostic} if diagnostic.code==DiagnosticCode::BudgetExceeded.as_str() && diagnostic.message=="original wasm callback budget"),
                        "{error:?}"
                    ),
                    _ => {
                        let OtterError::OutOfMemory {
                            requested_bytes,
                            heap_limit_bytes,
                        } = error
                        else {
                            panic!("{error:?}")
                        };
                        assert!(requested_bytes > heap_limit_bytes);
                        assert_eq!(heap_limit_bytes, 8 * 1024 * 1024);
                        assert_eq!(
                            records.lock().unwrap().oom,
                            Some((requested_bytes, heap_limit_bytes)),
                            "exact real allocator cause crosses typed trap"
                        );
                    }
                }
            }
            assert_eq!(
                eval_string(&mut runtime, "wasmCaught+'|'+wasmReturned"),
                "0|0"
            );
        }
    }
}

#[test]
fn wasm_intrinsic_classes_ignore_changed_namespace_constructors() {
    let mut runtime = runtime(Arc::new(Mutex::new(Records::default())));
    let trap = bytes("(module (func (export \"trap\") unreachable))");
    let imports = bytes(START);
    eval_string(
        &mut runtime,
        &format!(
            r#"
      const compileProto=WebAssembly.CompileError.prototype;
      const linkProto=WebAssembly.LinkError.prototype;
      const runtimeProto=WebAssembly.RuntimeError.prototype;
      const typeProto=TypeError.prototype;
      const trapModule=new WebAssembly.Module(new Uint8Array([{trap}]));
      const importModule=new WebAssembly.Module(new Uint8Array([{imports}]));
      const instance=new WebAssembly.Instance(trapModule);
      globalThis.errorCalls=0; globalThis.wasmClasses=[];
      WebAssembly.CompileError=WebAssembly.LinkError=WebAssembly.RuntimeError=TypeError=function changed(){{errorCalls++;throw 99;}};
      for(const [f,proto] of [
        [()=>new WebAssembly.Module(new Uint8Array([0,1,2])),compileProto],
        [()=>new WebAssembly.Instance(importModule),linkProto],
        [()=>instance.exports.trap(),runtimeProto],
        [()=>new WebAssembly.Module(null),typeProto]]){{
          try{{f();wasmClasses.push(false);}}catch(error){{wasmClasses.push(Object.getPrototypeOf(error)===proto && error instanceof Error);}}
      }}
      WebAssembly.compile(new Uint8Array([0,1,2])).catch(error=>wasmClasses.push(Object.getPrototypeOf(error)===compileProto));
      WebAssembly.instantiate(importModule).catch(error=>wasmClasses.push(Object.getPrototypeOf(error)===linkProto));
    "#
        ),
    );
    assert_eq!(
        eval_string(&mut runtime, "wasmClasses.join('|')+'|'+errorCalls"),
        "true|true|true|true|true|true|0"
    );
}

#[test]
fn wasm_catchable_host_class_and_getter_throw_keep_their_original_causes() {
    let mut runtime = runtime(Arc::new(Mutex::new(Records::default())));
    let binary = bytes(START);
    eval_string(
        &mut runtime,
        &format!(
            r#"
        const module=new WebAssembly.Module(new Uint8Array([{binary}]));
        const syntaxProto=SyntaxError.prototype; globalThis.wasmOut=[];
        SyntaxError=function changed(){{throw 94;}};
        try{{new WebAssembly.Instance(module,{{env:{{callback:()=>wasmFailure(99)}}}});}}
        catch(error){{wasmOut.push(Object.getPrototypeOf(error)===syntaxProto && error.message==='wasm failure: original syntax payload');}}
        wasmReset(); const original={{marker:731}}; original.self=original;
        const imports={{get env(){{wasmMove(original);throw original;}}}};
        try{{new WebAssembly.Instance(module,imports);}}catch(error){{wasmOut.push(error===original && error.self===original && error.marker===731);}}
        WebAssembly.instantiate(module,imports).catch(error=>wasmOut.push(error===original && error.self===original));
        wasmReset(); const prototypeCause={{marker:731}}; prototypeCause.self=prototypeCause;
        globalThis.wasmPrototypeStarts=0;
        Object.defineProperty(WebAssembly,'__instanceProto',{{configurable:true,get(){{wasmMove(prototypeCause);throw prototypeCause;}}}});
        try{{new WebAssembly.Instance(module,{{env:{{callback:()=>{{wasmPrototypeStarts++;}}}}}});}}
        catch(error){{wasmOut.push(error===prototypeCause && error.self===prototypeCause && error.marker===731);}}
    "#
        ),
    );
    assert_eq!(
        eval_string(&mut runtime, "wasmOut.join('|')+'|'+wasmPrototypeStarts"),
        "true|true|true|true|1"
    );
}

#[test]
fn wasm_externref_imports_and_jstag_roundtrip_moving_identity() {
    let binary = bytes(
        r#"(module
        (import "env" "callback" (func $callback (param externref) (result externref)))
        (import "env" "jsTag" (tag $jsTag (param externref)))
        (func (export "call") (param externref) (result externref)
          (block $caught (result externref)
            (try_table (result externref) (catch $jsTag $caught)
              local.get 0 call $callback)))
        (func (export "uncaught") (param externref) (result externref)
          local.get 0 call $callback))"#,
    );
    for (thrown, caught_in_wasm) in [(false, true), (true, true), (true, false)] {
        let records = Arc::new(Mutex::new(Records::default()));
        let mut runtime = runtime(records.clone());
        let callback = if thrown {
            "value=>{const moved=wasmMove(value);throw moved;}"
        } else {
            "value=>wasmMove(value)"
        };
        let invoke = if caught_in_wasm {
            "instance.exports.call(fresh)"
        } else {
            "(()=>{try{instance.exports.uncaught(fresh);return null;}catch(error){return error;}})()"
        };
        assert_eq!(
            eval_string(
                &mut runtime,
                &format!(
                    r#"
            const module=new WebAssembly.Module(new Uint8Array([{binary}]));
            const instance=new WebAssembly.Instance(module,{{env:{{callback:{callback},jsTag:WebAssembly.JSTag}}}});
            wasmReset(); const fresh={{marker:731}}; fresh.self=fresh; const alias=fresh;
            const result={invoke};
            [result===fresh,result===alias,result.self===fresh,result.marker].join('|');
        "#
                )
            ),
            "true|true|true|731"
        );
        let records = records.lock().unwrap();
        assert_eq!(records.motions.len(), 1);
        let motion = records.motions[0].as_ref().expect("actual scoped motion");
        assert_ne!(
            motion.before, motion.after,
            "young payload actually evacuated during import"
        );
        assert_eq!(motion.marker, 731.0);
        assert!(motion.minor_after > motion.minor_before);
    }
}

#[test]
fn wasm_error_materialization_failure_cannot_be_rejected_or_caught_as_undefined() {
    for asynchronous in [false, true] {
        let mut runtime = runtime(Arc::new(Mutex::new(Records::default())));
        let binary = bytes(START);
        let invoke = if asynchronous {
            "WebAssembly.instantiate(module,imports).then(()=>wasmHandled++,()=>wasmHandled++);"
        } else {
            "try{new WebAssembly.Instance(module,imports);wasmHandled++;}catch(error){wasmHandled++;}"
        };
        let error = runtime
            .run_script(
                SourceInput::from_javascript(format!(
                    r#"
            wasmReset();globalThis.wasmHandled=0;
            const module=new WebAssembly.Module(new Uint8Array([{binary}]));
            const imports={{env:{{callback:()=>wasmHugeError()}}}};
            {invoke}
        "#
                )),
                "wasm-materialize-oom.js",
            )
            .expect_err("actual oversized Error string fails materialization");
        assert!(
            matches!(error,OtterError::OutOfMemory{requested_bytes,heap_limit_bytes}
            if requested_bytes>heap_limit_bytes && heap_limit_bytes==8*1024*1024),
            "{error:?}"
        );
        assert_eq!(eval_string(&mut runtime, "wasmHandled"), "0");
    }
}

#[test]
fn wasm_pinned_intrinsics_keep_descriptors_and_realm_identity_after_full_gc() {
    let mut runtime = runtime(Arc::new(Mutex::new(Records::default())));
    let realm = runtime.create_realm().expect("additional Web realm");
    let trap = bytes("(module (func (export \"trap\") unreachable))");
    let source = format!(
        r#"
        globalThis.wasmOriginals=['CompileError','LinkError','RuntimeError'].map(name=>WebAssembly[name]);
        globalThis.wasmDescriptorProof=wasmOriginals.map(ctor=>{{
            const prototype=ctor.prototype;
            const c=Object.getOwnPropertyDescriptor(ctor,'prototype');
            const n=Object.getOwnPropertyDescriptor(prototype,'name');
            const e=new ctor('registry payload');
            const called=ctor('ordinary payload');
            return Object.getPrototypeOf(called)===prototype && called instanceof ctor &&
                called.message==='ordinary payload' && typeof ctor==='function' && ctor.length===1 &&
                Object.getPrototypeOf(ctor)===Error && Object.getPrototypeOf(prototype)===Error.prototype &&
                !c.writable && !c.enumerable && !c.configurable && n.writable && !n.enumerable && n.configurable &&
                e instanceof ctor && e instanceof Error && e.message==='registry payload' && e.name===ctor.name &&
                Object.prototype.toString.call(prototype)==='[object Object]' &&
                Object.prototype.toString.call(e)==='[object Error]' && typeof globalThis[ctor.name]==='undefined';
        }}).join('|');
        globalThis.wasmTrapInstance=new WebAssembly.Instance(new WebAssembly.Module(new Uint8Array([{trap}])));
        WebAssembly.CompileError=WebAssembly.LinkError=WebAssembly.RuntimeError=function changed(){{throw 91;}};
        wasmDescriptorProof;
    "#
    );
    assert_eq!(eval_string(&mut runtime, &source), "true|true|true");
    let additional = runtime
        .run_script_in_realm(
            realm,
            SourceInput::from_javascript(source),
            "wasm-registry-realm.js",
        )
        .expect("additional realm pinned intrinsics");
    assert_eq!(additional.completion_string(), "true|true|true");
    runtime
        .force_gc()
        .expect("real collection over both realms");
    let probe = "(()=>{try{wasmTrapInstance.exports.trap();return false;}catch(error){return Object.getPrototypeOf(error)===wasmOriginals[2].prototype && error instanceof Error && wasmOriginals.every(ctor=>Object.getPrototypeOf(ctor)===Error);}})()";
    assert_eq!(eval_string(&mut runtime, probe), "true");
    let additional = runtime
        .run_script_in_realm(
            realm,
            SourceInput::from_javascript(probe),
            "wasm-registry-realm-after-gc.js",
        )
        .expect("additional realm trap after collection");
    assert_eq!(additional.completion_string(), "true");
}
