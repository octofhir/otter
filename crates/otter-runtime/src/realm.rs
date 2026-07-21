//! High-level runtime realm embedding.
//!
//! # Contents
//! - [`RuntimeRealmId`] — opaque stable identity for one realm.
//! - [`RuntimeGlobalValue`] — owned primitive accepted by safe installers.
//! - [`RuntimeRealmContext`] — safe installer surface shared by the default
//!   realm and additional realms.
//! - Configured class/extension installation for a newly active realm.
//!
//! # Invariants
//! - Public realm APIs never expose interpreter, value, object, or GC handles.
//! - Realm ids are scalar identities; all moving handles remain traced by the
//!   interpreter.
//! - Installer callbacks can add globals and run bootstrap source, but cannot
//!   reach raw heap mutation or retain an isolate-local borrow.
//!
//! # See also
//! - [`crate::RuntimeGlobalInstaller`]
//! - [`crate::RuntimeHandle`]

use crate::{
    CapabilitySet, DiagnosticCode, GlobalClassInner, OtterError, RuntimeConfig, RuntimeHooks,
    RuntimeNativeCall, RuntimeNativeFastFn, RuntimeTaskSpawner, SourceInput,
};

/// Opaque identity for one additional realm owned by a runtime isolate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RuntimeRealmId(pub(crate) otter_vm::HostRealmId);

/// Owned primitive that can be installed without exposing VM values.
#[derive(Debug, Clone, PartialEq)]
pub enum RuntimeGlobalValue {
    /// JavaScript `undefined`.
    Undefined,
    /// JavaScript `null`.
    Null,
    /// A JavaScript boolean.
    Boolean(bool),
    /// A JavaScript number.
    Number(f64),
    /// A JavaScript string copied into the target realm.
    String(String),
}

impl From<bool> for RuntimeGlobalValue {
    fn from(value: bool) -> Self {
        Self::Boolean(value)
    }
}

impl From<f64> for RuntimeGlobalValue {
    fn from(value: f64) -> Self {
        Self::Number(value)
    }
}

impl From<i32> for RuntimeGlobalValue {
    fn from(value: i32) -> Self {
        Self::Number(f64::from(value))
    }
}

impl From<String> for RuntimeGlobalValue {
    fn from(value: String) -> Self {
        Self::String(value)
    }
}

impl From<&str> for RuntimeGlobalValue {
    fn from(value: &str) -> Self {
        Self::String(value.to_owned())
    }
}

/// Safe, high-level surface passed to configured global installers.
///
/// The context is valid only for the installer call. It deliberately exposes
/// no raw JS values, objects, interpreter access, or GC operations.
pub struct RuntimeRealmContext<'a> {
    interp: &'a mut otter_vm::Interpreter,
    capabilities: &'a CapabilitySet,
    hooks: &'a RuntimeHooks,
    runtime_task_spawner: Option<RuntimeTaskSpawner>,
}

impl<'a> RuntimeRealmContext<'a> {
    pub(crate) fn new(
        interp: &'a mut otter_vm::Interpreter,
        capabilities: &'a CapabilitySet,
        hooks: &'a RuntimeHooks,
        runtime_task_spawner: Option<RuntimeTaskSpawner>,
    ) -> Self {
        Self {
            interp,
            capabilities,
            hooks,
            runtime_task_spawner,
        }
    }

    /// Configured capability snapshot for host closures installed in this realm.
    #[must_use]
    pub fn capabilities(&self) -> &CapabilitySet {
        self.capabilities
    }

    /// Owned task-delivery handle for async host closures, when Layer B is active.
    #[must_use]
    pub fn runtime_task_spawner(&self) -> Option<RuntimeTaskSpawner> {
        self.runtime_task_spawner.clone()
    }

    /// Install an owned primitive as a realm global.
    ///
    /// This is the default data-injection path for embedders. The value is
    /// materialized inside the active realm, so no VM or GC handle crosses the
    /// public boundary.
    pub fn install_global(
        &mut self,
        name: &str,
        value: impl Into<RuntimeGlobalValue>,
    ) -> Result<(), OtterError> {
        let value = match value.into() {
            RuntimeGlobalValue::Undefined => otter_vm::Value::undefined(),
            RuntimeGlobalValue::Null => otter_vm::Value::null(),
            RuntimeGlobalValue::Boolean(value) => otter_vm::Value::boolean(value),
            RuntimeGlobalValue::Number(value) => otter_vm::Value::number_f64(value),
            RuntimeGlobalValue::String(value) => {
                let value = otter_vm::JsString::from_str(&value, self.interp.gc_heap_mut())
                    .map_err(crate::string_oom_to_error)?;
                otter_vm::Value::string(value)
            }
        };
        self.interp.set_global(name, value);
        Ok(())
    }

    /// Install a static native function as a realm global.
    pub fn install_native_global(
        &mut self,
        name: &'static str,
        length: u8,
        call: RuntimeNativeFastFn,
    ) -> Result<(), OtterError> {
        let value = self
            .interp
            .native_function_static_host_rooted(name, length, call, &[], &[])
            .map_err(|oom| OtterError::OutOfMemory {
                requested_bytes: oom.requested_bytes(),
                heap_limit_bytes: oom.heap_limit_bytes(),
            })?;
        self.interp.set_global(name, value);
        Ok(())
    }

    /// Install a captured native call target as a realm global.
    pub fn install_native_global_call(
        &mut self,
        name: &'static str,
        length: u8,
        call: RuntimeNativeCall,
    ) -> Result<(), OtterError> {
        let value = self
            .interp
            .native_function_from_call_host_rooted(name, length, call, &[], &[])
            .map_err(|oom| OtterError::OutOfMemory {
                requested_bytes: oom.requested_bytes(),
                heap_limit_bytes: oom.heap_limit_bytes(),
            })?;
        self.interp.set_global(name, value);
        Ok(())
    }

    /// Execute trusted bootstrap source in the active realm.
    ///
    /// This surface is for extension installation. Page code should use
    /// [`crate::Runtime::run_script_in_realm`] or the corresponding handle API.
    pub fn install_script(&mut self, source: SourceInput) -> Result<(), OtterError> {
        let compiled = if let Some(hook) = self.hooks.compile_hook() {
            let resolved = crate::module_loader::ResolvedSource {
                url: "<realm-installer>".to_string(),
                kind: source.kind,
                jsx: None,
                text: source.text,
            };
            hook.compile(crate::RuntimeCompileRequest { source: &resolved })?
        } else {
            otter_compiler::compile_script_source_to_module(
                &source.text,
                source.kind,
                "<realm-installer>",
            )
            .map_err(|error| crate::map_compile_error(error, "<realm-installer>"))?
        };
        let context = self.interp.link_module(compiled.bytecode);
        self.interp.run(&context).map_err(crate::map_vm_error)?;
        self.interp
            .drain_microtasks(&context)
            .map_err(crate::map_vm_error)
    }
}

pub(crate) struct PendingRealmScripts {
    pub(crate) class_js: Vec<(&'static str, &'static str)>,
    pub(crate) extension_js: Vec<(String, String)>,
}

pub(crate) fn install_class_surfaces(
    interp: &mut otter_vm::Interpreter,
    config: &RuntimeConfig,
) -> Result<PendingRealmScripts, OtterError> {
    let mut class_js = Vec::new();
    let mut extension_js = Vec::new();
    for spec in &config.global_classes {
        install_class(interp, spec.inner, &mut class_js)?;
    }
    for extension in &config.extensions {
        for spec in extension.classes {
            install_class(interp, spec.inner, &mut class_js)?;
        }
        if !extension.js.is_empty() {
            let mut source = String::new();
            for entry in extension.js {
                source.push_str(entry.source);
                source.push_str("\n;\n");
            }
            extension_js.push((extension.name.to_string(), source));
        }
    }
    Ok(PendingRealmScripts {
        class_js,
        extension_js,
    })
}

fn install_class(
    interp: &mut otter_vm::Interpreter,
    class: GlobalClassInner,
    pending_js: &mut Vec<(&'static str, &'static str)>,
) -> Result<(), OtterError> {
    match class {
        GlobalClassInner::Spec(raw) => {
            interp
                .install_global_class(raw)
                .map_err(|error| OtterError::Internal {
                    code: DiagnosticCode::GlobalClassBootstrap.as_str().to_string(),
                    message: error.to_string(),
                })
        }
        GlobalClassInner::Intrinsic {
            install,
            install_well_knowns,
            js_glue,
            name,
        } => {
            if let Some(source) = js_glue {
                pending_js.push((name, source));
            }
            let global = *interp.global_this();
            install(interp.gc_heap_mut(), global).map_err(|error| OtterError::Internal {
                code: DiagnosticCode::GlobalClassBootstrap.as_str().to_string(),
                message: error.to_string(),
            })?;
            interp
                .run_install_well_knowns(install_well_knowns, global)
                .map_err(|error| OtterError::Internal {
                    code: DiagnosticCode::GlobalClassBootstrap.as_str().to_string(),
                    message: error.to_string(),
                })
        }
    }
}

impl crate::Runtime {
    /// Create and fully bootstrap an additional realm in this isolate.
    ///
    /// The realm receives the runtime's configured classes, extensions and
    /// high-level global installers. Isolate-wide product singletons such as
    /// `process` and the worker controller are not replayed automatically.
    /// Browser concepts remain entirely in embedder-provided extensions.
    pub fn create_realm(&mut self) -> Result<RuntimeRealmId, OtterError> {
        let realm = self
            .interp
            .create_host_realm()
            .map_err(map_realm_vm_error)?;
        let config = self.config.clone();
        let task_spawner = self.runtime_task_spawner.clone();
        let installed: Result<(), OtterError> = self
            .interp
            .with_host_realm(realm, |interp| {
                Ok(interp.with_runtime_roots(|interp| {
                    let pending = install_class_surfaces(interp, &config)?;
                    let mut context = RuntimeRealmContext::new(
                        interp,
                        &config.capabilities,
                        &config.hooks,
                        task_spawner,
                    );
                    for installer in &config.global_installers {
                        installer.install(&mut context)?;
                    }
                    for (name, source) in pending.extension_js {
                        context
                            .install_script(SourceInput::from_javascript(source))
                            .map_err(|error| OtterError::Internal {
                                code: DiagnosticCode::GlobalClassBootstrap.as_str().to_string(),
                                message: format!("extension `{name}` globals failed: {error}"),
                            })?;
                    }
                    for (name, source) in pending.class_js {
                        context
                            .install_script(SourceInput::from_javascript(source))
                            .map_err(|error| OtterError::Internal {
                                code: DiagnosticCode::GlobalClassBootstrap.as_str().to_string(),
                                message: format!("class `{name}` attached JS glue failed: {error}"),
                            })?;
                    }
                    Ok(())
                }))
            })
            .map_err(map_realm_vm_error)?;
        installed?;
        Ok(RuntimeRealmId(realm))
    }

    /// Compile and execute a classic script in an additional realm.
    ///
    /// The result is an owned [`crate::ExecutionResult`]; no realm-local value
    /// or execution context crosses the public boundary.
    pub fn run_script_in_realm(
        &mut self,
        realm: RuntimeRealmId,
        source: SourceInput,
        specifier: &str,
    ) -> Result<crate::ExecutionResult, OtterError> {
        self.interp.begin_jit_debug_capture();
        let started = std::time::Instant::now();
        let compiled = self.compile_source(&source, specifier)?;
        let result = self
            .interp
            .with_host_realm(realm.0, |interp| {
                let context = interp.link_module(compiled.bytecode);
                let script = interp.run(&context);
                let checkpoint = interp.drain_microtasks(&context);
                match (script, checkpoint) {
                    (
                        Err(otter_vm::RunError {
                            error: otter_vm::VmError::Exit { code },
                            ..
                        }),
                        _,
                    )
                    | (
                        Ok(_),
                        Err(otter_vm::RunError {
                            error: otter_vm::VmError::Exit { code },
                            ..
                        }),
                    ) => Ok(Ok(crate::ExecutionResult::from_exit_code(
                        code,
                        started.elapsed(),
                    ))),
                    (Err(error), _) | (Ok(_), Err(error)) => Ok(Err(error)),
                    (Ok(value), Ok(())) => Ok(Ok(crate::ExecutionResult::from_vm_value(
                        value,
                        started.elapsed(),
                        interp.gc_heap_mut(),
                    )
                    .with_exit_code(crate::process::exit_code(interp)))),
                }
            })
            .map_err(map_realm_vm_error)?
            .map_err(crate::map_vm_error)?;
        let result = self.attach_execution_stats(result);
        Ok(self.attach_jit_debug_report(result))
    }
}

fn map_realm_vm_error(error: otter_vm::VmError) -> OtterError {
    crate::map_vm_error(otter_vm::RunError {
        error,
        frames: Vec::new(),
        detail: None,
    })
}
