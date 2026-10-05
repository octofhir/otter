//! Hosted-module preparation and active-realm completion projection.
//!
//! # Contents
//! - Canonical entry self-resolution edges and one admitted dynamic-import graph
//!   recipe shared by default/additional realms.
//! - Static entry projection through the existing owned native completion owner.
//! - Dynamic rejection materialization through the existing scoped intrinsic owner.
//!
//! # Invariants
//! - Module-record allocation returns its original typed native failure. The
//!   consumer projects it before leaving the selected realm and while pending
//!   thrown identity, source frames and detail still belong to that completion.
//! - Completed terminal failures and escaping actual allocation refusals leave
//!   the turn through the canonical runtime mapper. They never become catchable
//!   dynamic-import diagnostics or execute a rejection materializer.
//! - Catchable errors use the one MarshalCx error materializer. User throws keep
//!   their pending rooted value; only native specification errors create an
//!   intrinsic error in the active realm. Materialization failure remains fatal.
//! - The graph's wrapper is actual namespace-import source with an admitted
//!   source lease and initializer. It is not an ambient native execution context.
//!   RuntimeModuleRecords alone owns realm-local environment/cache publication.
//!
//! # See also
//! - `crate::module_records` owns installers and traced environment publication.
//! - `otter_vm::NativeCtx::take_native_error` owns exact completed diagnostics.
//! - `otter_vm::marshal::MarshalCx::error_value` owns throwable materialization.

use crate::{DynLoadError, OtterError, module_graph, module_loader, module_records};
use otter_vm::{Interpreter, NativeCallInfo, NativeCtx, NativeError};

/// Prepare the entry driver's self-resolution edges before verified linking.
/// Existing realm records and this incoming graph contribute canonical URLs;
/// no environment or initializer is published until the linked ids are final.
pub(super) fn prepare_entry_resolutions(
    records: &module_records::RuntimeModuleRecords,
    realm_id: u32,
    module: &mut otter_bytecode::BytecodeModule,
    entry_url: &str,
) {
    let mut urls: std::collections::BTreeSet<String> = module
        .module_inits
        .iter()
        .map(|init| init.url.clone())
        .collect();
    records.for_each_record(realm_id, |url, _| {
        urls.insert(url.to_owned());
    });
    for url in urls {
        for referrer in [entry_url, ""] {
            module
                .module_resolutions
                .push(otter_bytecode::ModuleResolution {
                    referrer: referrer.to_owned(),
                    specifier: url.clone(),
                    attr_type: None,
                    target: url.clone(),
                    deferred: false,
                    dynamic: false,
                    synthetic: true,
                });
        }
    }
}

pub(super) fn prepare_dynamic_graph(
    loader: &module_loader::ModuleLoader,
    target_url: &str,
) -> Result<module_graph::LinkedProgram, module_graph::GraphError> {
    let entry_url = format!("otter-hosted-dynamic:{target_url}");
    let specifier = serde_json::to_string(target_url).map_err(|error| {
        module_graph::GraphError::Resolution {
            url: target_url.to_owned(),
            message: error.to_string(),
        }
    })?;
    let text = format!(
        "import * as __otterHosted from {specifier};\n\
         export default __otterHosted;\n"
    );
    let text = module_loader::admit_source(loader.resource_account(), &entry_url, text)?;
    module_graph::load_program_source(
        loader,
        module_loader::ResolvedSource {
            url: entry_url,
            kind: otter_syntax::SourceKind::JavaScript,
            jsx: None,
            text,
        },
    )
}

pub(super) fn into_runtime(interp: &mut Interpreter, error: NativeError) -> OtterError {
    let completion =
        NativeCtx::with_host_context(interp, NativeCallInfo::default_call(), None, |native| {
            native.take_native_error(error)
        });
    crate::enrich_runtime_diagnostic_with_cause(interp, crate::map_vm_error(completion))
}

pub(super) fn into_dynamic(interp: &mut Interpreter, error: NativeError) -> DynLoadError {
    NativeCtx::with_host_context(interp, NativeCallInfo::default_call(), None, |native| {
        // This host installation has completed. Source-level authored native
        // OOM catchability is unchanged; an OOM escaping installation stops it.
        if error.is_fatal() || matches!(error, NativeError::OutOfMemory { .. }) {
            return DynLoadError::Fatal(crate::map_vm_error(native.take_native_error(error)));
        }
        let value = native.scope(|scope| {
            let mut marshal = otter_vm::marshal::MarshalCx::new(scope);
            let reason = marshal.error_value(otter_vm::marshal::JsError::Native(error))?;
            Ok::<_, otter_vm::marshal::JsError>(marshal.escape(reason))
        });
        match value {
            Ok(value) => DynLoadError::Thrown(value),
            Err(error) => {
                let error = error.into_native("hosted module completion");
                DynLoadError::Fatal(crate::map_vm_error(native.take_native_error(error)))
            }
        }
    })
}
