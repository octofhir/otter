//! Actual script/checkpoint fatal policy through all direct Runtime entry families.
//!
//! # Contents
//! - Invalid operand, missing return, budget, interruption and exit completions.
//! - Default/additional classic scripts and in-memory module entry execution.
//! - Catchable-error checkpoint semantics and explicit later-turn queue recovery.
//! - Original queued source-context retention and fatal drain handler exclusion.
//!
//! # Invariants
//! - Installed native functions enter the real VM error projection; no raw JIT
//!   ABI, synthetic pending fatal state or alternate job queue is involved.
//! - Callbacks record owned console text. Assertions execute outside native ABI.
//! - Each case uses a fresh runtime. Recovering on it is a deliberate host action,
//!   and verifies the old job executes after the new script, exactly once.

use std::sync::{Arc, Mutex};

use otter_runtime::{
    ConsoleLevel, ConsoleSink, DiagnosticCode, NativeError, OtterError, Runtime,
    RuntimeExtensionInstaller, RuntimeNativeCall, RuntimeRealmId, SourceInput,
};

#[derive(Debug, Default)]
struct Capture {
    events: Mutex<Vec<String>>,
    allocation_failure: Mutex<Option<(u64, u64)>>,
}

impl ConsoleSink for Capture {
    fn write(&self, level: ConsoleLevel, fields: &[String]) {
        if matches!(level, ConsoleLevel::Log) {
            self.events
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(fields.join(" "));
        }
    }
}

impl Capture {
    fn snapshot(&self) -> Vec<String> {
        self.events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

fn runtime(capture: Arc<Capture>) -> Runtime {
    Runtime::builder()
        .console_sink(capture.clone())
        .max_heap_bytes(8 * 1024 * 1024)
        .extension_installer(RuntimeExtensionInstaller::new(move |realm| {
            realm.install_native_global("checkpointInvalid", 0, |_ctx, _args| {
                Err(NativeError::InvalidOperand)
            })?;
            realm.install_native_global("checkpointMissing", 0, |_ctx, _args| {
                Err(NativeError::MissingReturn)
            })?;
            realm.install_native_global("checkpointBudget", 0, |_ctx, _args| {
                Err(NativeError::BudgetExceeded {
                    reason: "checkpoint original budget".into(),
                })
            })?;
            realm.install_native_global("checkpointInterrupted", 0, |_ctx, _args| {
                Err(NativeError::Interrupted)
            })?;
            realm.install_native_global("checkpointExit", 0, |_ctx, _args| {
                Err(NativeError::Exit { code: 27 })
            })?;
            realm.install_native_global("checkpointReportedOOM", 0, |_ctx, _args| {
                // An authored native OOM has its original catchable VM contract.
                Err(NativeError::OutOfMemory {
                    name: "checkpointReportedOOM",
                    requested_bytes: 817,
                    heap_limit_bytes: 8 * 1024 * 1024,
                })
            })?;
            let allocation_capture = capture.clone();
            realm.install_native_global_call(
                "checkpointAllocate",
                0,
                RuntimeNativeCall::Dynamic(Arc::new(move |ctx, _args, _state| {
                    ctx.scope(|mut scope| {
                        // A real request larger than the configured engine heap cap.
                        let _value = scope.string(&"x".repeat(16 * 1024 * 1024))?;
                        Ok(otter_runtime::Value::undefined())
                    })
                    .map_err(|error| {
                        if let NativeError::ExecutionFailure(otter_vm::RunError {
                            error:
                                otter_vm::VmError::OutOfMemory {
                                    requested_bytes,
                                    heap_limit_bytes,
                                },
                            ..
                        }) = &error
                        {
                            *allocation_capture
                                .allocation_failure
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                                Some((*requested_bytes, *heap_limit_bytes));
                        }
                        error
                    })
                })),
            )?;
            Ok(())
        }))
        .build()
        .expect("checkpoint runtime")
}

#[derive(Clone, Copy, Debug)]
enum Entry {
    Script,
    RealmScript,
    Module,
    RealmModule,
}

impl Entry {
    const ALL: [Self; 4] = [
        Self::Script,
        Self::RealmScript,
        Self::Module,
        Self::RealmModule,
    ];

    fn realm(self, runtime: &mut Runtime) -> Option<RuntimeRealmId> {
        matches!(self, Self::RealmScript | Self::RealmModule)
            .then(|| runtime.create_realm().expect("checkpoint additional realm"))
    }

    fn execute(
        self,
        runtime: &mut Runtime,
        realm: Option<RuntimeRealmId>,
        source: &str,
        name: &str,
    ) -> Result<otter_runtime::ExecutionResult, OtterError> {
        let source = SourceInput::from_javascript(source);
        match self {
            Self::Script => runtime.run_script(source, name),
            Self::RealmScript => runtime.run_script_in_realm(realm.unwrap(), source, name),
            Self::Module => runtime.run_module_source(source, format!("file:///{name}")),
            Self::RealmModule => runtime.run_module_source_in_realm(
                realm.unwrap(),
                source,
                format!("file:///{name}"),
            ),
        }
    }

    fn recover(
        self,
        runtime: &mut Runtime,
        realm: Option<RuntimeRealmId>,
    ) -> Result<otter_runtime::ExecutionResult, OtterError> {
        // A later classic script avoids revisiting an errored module record.
        match realm {
            Some(realm) => runtime.run_script_in_realm(
                realm,
                SourceInput::from_javascript("console.log('later-script'); 41;"),
                "deliberate-recovery.js",
            ),
            None => runtime.run_script(
                SourceInput::from_javascript("console.log('later-script'); 41;"),
                "deliberate-recovery.js",
            ),
        }
    }
}

fn assert_failure(error: OtterError, expected: &str, capture: &Capture) {
    if expected == "checkpointAllocate" {
        let actual = *capture
            .allocation_failure
            .lock()
            .expect("actual allocation failure");
        assert!(
            actual.is_some(),
            "native boundary must observe a real allocator failure"
        );
        let OtterError::OutOfMemory {
            requested_bytes,
            heap_limit_bytes,
        } = &error
        else {
            panic!("allocator failure changed family: {error:?}");
        };
        assert_eq!(
            Some((*requested_bytes, *heap_limit_bytes)),
            actual,
            "the actual allocation cause must survive the VM and Runtime boundary"
        );
    }
    match expected {
        "checkpointInvalid" | "checkpointMissing" => assert!(
            matches!(error, OtterError::Internal { ref code, ref message }
                if code == DiagnosticCode::VmBytecodeInvariant.as_str()
                && message == &if expected == "checkpointInvalid" {
                    otter_vm::VmError::InvalidOperand.to_string()
                } else { otter_vm::VmError::MissingReturn.to_string() }),
            "{error:?}"
        ),
        "checkpointBudget" => assert!(
            matches!(error, OtterError::Runtime { ref diagnostic }
                if diagnostic.code == DiagnosticCode::BudgetExceeded.as_str()
                && diagnostic.message == "checkpoint original budget"),
            "{error:?}"
        ),
        "checkpointInterrupted" => assert!(matches!(error, OtterError::Interrupted), "{error:?}"),
        "checkpointAllocate" => assert!(
            matches!(error, OtterError::OutOfMemory { requested_bytes, heap_limit_bytes }
                if requested_bytes > heap_limit_bytes && heap_limit_bytes == 8 * 1024 * 1024),
            "{error:?}"
        ),
        _ => panic!("unknown fixture failure {expected}: {error:?}"),
    }
}

#[test]
fn fatal_scripts_skip_current_checkpoint_and_deliberate_later_turn_resumes_owned_jobs() {
    for entry in Entry::ALL {
        for name in [
            "checkpointInvalid",
            "checkpointMissing",
            "checkpointBudget",
            "checkpointInterrupted",
            "checkpointExit",
            "checkpointAllocate",
        ] {
            let capture = Arc::new(Capture::default());
            let mut runtime = runtime(capture.clone());
            let realm = entry.realm(&mut runtime);
            let source = format!(
                "(() => {{ const original = '{entry:?}:{name}'; \
                 queueMicrotask(() => console.log('retained-original:' + original)); }})(); \
                 {name}(); console.log('after-fatal');"
            );
            let before = runtime.work_budget_stats();
            let outcome = entry.execute(&mut runtime, realm, &source, "first-fatal.js");
            if name == "checkpointExit" {
                assert_eq!(outcome.expect("explicit exit outcome").exit_code(), 27);
            } else {
                assert_failure(
                    outcome.expect_err("original fatal must escape"),
                    name,
                    &capture,
                );
            }
            assert!(
                capture.snapshot().is_empty(),
                "{entry:?} {name} entered queued JS"
            );
            let after = runtime.work_budget_stats();
            assert_eq!(
                after.microtask_drains, before.microtask_drains,
                "{entry:?} {name}"
            );
            assert_eq!(
                after.microtasks_executed, before.microtasks_executed,
                "{entry:?} {name}"
            );
            entry
                .recover(&mut runtime, realm)
                .expect("deliberate direct-runtime recovery");
            assert_eq!(
                capture.snapshot(),
                vec![
                    "later-script".to_string(),
                    format!("retained-original:{entry:?}:{name}"),
                ]
            );
            assert_eq!(
                runtime.work_budget_stats().microtasks_executed - after.microtasks_executed,
                1
            );
        }
    }
}

#[test]
fn catchable_script_throws_keep_the_existing_checkpoint_and_original_error_precedence() {
    for entry in Entry::ALL {
        let capture = Arc::new(Capture::default());
        let mut runtime = runtime(capture.clone());
        let realm = entry.realm(&mut runtime);
        let error = entry.execute(&mut runtime, realm,
            "queueMicrotask(() => console.log('catchable-checkpoint')); throw new SyntaxError('original script throw');",
            "catchable-first.js").expect_err("original script throw");
        assert!(
            matches!(error, OtterError::Runtime { ref diagnostic }
            if diagnostic.message.contains("original script throw")),
            "{error:?}"
        );
        assert_eq!(capture.snapshot(), vec!["catchable-checkpoint"]);
    }
}

#[test]
fn fatal_drain_returns_original_failure_without_uncaught_handler_and_retains_following_jobs() {
    for name in [
        "checkpointInvalid",
        "checkpointMissing",
        "checkpointBudget",
        "checkpointAllocate",
    ] {
        let capture = Arc::new(Capture::default());
        let mut runtime = runtime(capture.clone());
        let source = format!(
            "process.on('uncaughtException', () => console.log('handler-must-not-run')); \
             queueMicrotask(() => {name}()); queueMicrotask(() => console.log('retained-after-drain'));"
        );
        let error = runtime
            .run_script(SourceInput::from_javascript(&source), "fatal-drain.js")
            .expect_err("fatal drain cannot be claimed by JavaScript handler");
        assert_failure(error, name, &capture);
        assert!(capture.snapshot().is_empty());
        Entry::Script
            .recover(&mut runtime, None)
            .expect("later direct-runtime recovery");
        assert_eq!(
            capture.snapshot(),
            vec!["later-script", "retained-after-drain"]
        );
    }
}

#[test]
fn retained_job_error_uses_its_original_source_context_after_deliberate_later_turn() {
    for entry in Entry::ALL {
        let capture = Arc::new(Capture::default());
        let mut runtime = runtime(capture.clone());
        let realm = entry.realm(&mut runtime);
        let error = entry.execute(&mut runtime, realm,
            "queueMicrotask(function retainedOriginalSource() { throw new SyntaxError('original queued source'); }); checkpointInvalid();",
            "retained-original.js").expect_err("first fatal");
        assert_failure(error, "checkpointInvalid", &capture);
        let error = entry
            .recover(&mut runtime, realm)
            .expect_err("original queued task now runs");
        let OtterError::Runtime { diagnostic } = error else {
            panic!("{error:?}");
        };
        assert!(
            diagnostic.message.contains("original queued source"),
            "{diagnostic:?}"
        );
        assert!(
            diagnostic
                .frames
                .iter()
                .any(|frame| frame.function == "retainedOriginalSource"
                    && frame.module.ends_with("retained-original.js")),
            "{diagnostic:?}"
        );
        assert_eq!(capture.snapshot(), vec!["later-script"]);
    }
}

#[test]
fn caught_original_native_oom_permits_the_successful_script_checkpoint() {
    for entry in Entry::ALL {
        let capture = Arc::new(Capture::default());
        let mut runtime = runtime(capture.clone());
        let realm = entry.realm(&mut runtime);
        entry.execute(&mut runtime, realm,
            "try { checkpointReportedOOM(); } catch (error) { console.log(error instanceof RangeError); } queueMicrotask(() => console.log('after-caught-oom'));",
            "caught-native-oom.js").expect("caught original OOM is a successful script");
        assert_eq!(capture.snapshot(), vec!["true", "after-caught-oom"]);
    }
}
