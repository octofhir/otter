//! Core microtask scheduling through the default and Web-enabled runtime.
//!
//! # Contents
//! - Global descriptor, callback validation, indirect calls and job FIFO.
//! - Web installation preserves the already installed core callable.
//!
//! # Invariants
//! - No Web extension or timer scheduler is needed to enqueue a microtask.
//! - Jobs receive no arguments and use the existing realm-owned queue.
//! - Extension installation does not replace the core function identity.
//!
//! # See also
//! - `otter-difftest/corpus/core_microtask_queue.js` owns the shared JS contract.

use otter_runtime::{
    JitSelection, OtterError, Runtime, RuntimeExtensionContext, RuntimeExtensionInstaller,
    SourceInput,
};
use otter_web::WebApiBuilderExt;

#[test]
fn default_runtime_has_one_queue_for_direct_indirect_and_promise_jobs() {
    let source = include_str!("../../otter-difftest/corpus/core_microtask_queue.js");
    for selection in [
        JitSelection::InterpreterOnly,
        JitSelection::Template,
        JitSelection::ProductionTiered,
    ] {
        let mut runtime = Runtime::builder().jit_selection(selection).build().unwrap();
        let setup = runtime
            .run_script(
                SourceInput::from_javascript(source),
                "queue-microtask-core.js",
            )
            .expect("core scheduling without a Web installer");
        assert_eq!(
            setup.completion_string(),
            "ok:core-microtask",
            "{selection:?}"
        );
        let result = runtime
            .eval(SourceInput::from_javascript("JSON.stringify(jobs)"))
            .expect("read the drained core queue");
        assert_eq!(
            result.completion_string(),
            r#"[["q1",true,0],"p","q2",["proxy",true,0],"body",["bound",true],"nested"]"#,
            "{selection:?}"
        );
    }
}

fn remember_core_queue(realm: &mut RuntimeExtensionContext<'_>) -> Result<(), OtterError> {
    realm
        .install_script(SourceInput::from_javascript(
            "globalThis.coreQueueBeforeWeb = globalThis.queueMicrotask;",
        ))
        .map(|_| ())
}

#[test]
fn web_installer_preserves_the_core_microtask_callable() {
    let mut runtime = Runtime::builder()
        .extension_installer(RuntimeExtensionInstaller::new(remember_core_queue))
        .with_web_apis()
        .build()
        .expect("Web-enabled runtime");
    let result = runtime
        .eval(SourceInput::from_javascript(
            "typeof coreQueueBeforeWeb === 'function' && coreQueueBeforeWeb === queueMicrotask",
        ))
        .expect("compare core callable identity after Web installation");
    assert_eq!(result.completion_string(), "true");
}
