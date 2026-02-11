//! Stub `node:url` extension.
//!
//! Provides minimal exports needed by Node.js test harness (`common/tmpdir.js`):
//! `pathToFileURL`, `fileURLToPath`.

use otter_macros::{js_class, js_static};
use otter_vm_core::context::NativeContext;
use otter_vm_core::error::VmError;
use otter_vm_core::gc::GcRef;
use otter_vm_core::object::JsObject;
use otter_vm_core::string::JsString;
use otter_vm_core::value::Value;
use otter_vm_runtime::extension_v2::{OtterExtension, Profile};
use otter_vm_runtime::registration::RegistrationContext;

use std::sync::Arc;

// ---------------------------------------------------------------------------
// OtterExtension
// ---------------------------------------------------------------------------

pub struct NodeUrlExtension;

impl OtterExtension for NodeUrlExtension {
    fn name(&self) -> &str {
        "node_url"
    }

    fn profiles(&self) -> &[Profile] {
        static P: [Profile; 2] = [Profile::SafeCore, Profile::Full];
        &P
    }

    fn deps(&self) -> &[&str] {
        &[]
    }

    fn module_specifiers(&self) -> &[&str] {
        static S: [&str; 2] = ["node:url", "url"];
        &S
    }

    fn install(&self, _ctx: &mut RegistrationContext) -> Result<(), VmError> {
        Ok(())
    }

    fn load_module(
        &self,
        _specifier: &str,
        ctx: &mut RegistrationContext,
    ) -> Option<GcRef<JsObject>> {
        type DeclFn = fn() -> (
            &'static str,
            Arc<
                dyn Fn(&Value, &[Value], &mut NativeContext) -> Result<Value, VmError>
                    + Send
                    + Sync,
            >,
            u32,
        );

        let fns: &[DeclFn] = &[
            UrlStub::path_to_file_url_decl,
            UrlStub::file_url_to_path_decl,
        ];

        let mut ns = ctx.module_namespace();
        for decl in fns {
            let (name, func, length) = decl();
            ns = ns.function(name, func, length);
        }

        Some(ns.build())
    }
}

pub fn node_url_extension() -> Box<dyn OtterExtension> {
    Box::new(NodeUrlExtension)
}

// ---------------------------------------------------------------------------
// Stub functions via #[js_class]
// ---------------------------------------------------------------------------

#[js_class(name = "UrlStub")]
pub struct UrlStub;

#[js_class]
impl UrlStub {
    #[js_static(name = "pathToFileURL", length = 1)]
    pub fn path_to_file_url(
        _this: &Value,
        args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        let path = args
            .first()
            .and_then(|v| v.as_string())
            .map(|s| s.as_str().to_string())
            .unwrap_or_default();

        // Simple conversion: file:// + absolute path
        let url = if path.starts_with('/') {
            format!("file://{path}")
        } else {
            format!("file:///{path}")
        };

        Ok(Value::string(JsString::new_gc(&url)))
    }

    #[js_static(name = "fileURLToPath", length = 1)]
    pub fn file_url_to_path(
        _this: &Value,
        args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        let url = args
            .first()
            .and_then(|v| v.as_string())
            .map(|s| s.as_str().to_string())
            .unwrap_or_default();

        let path = url
            .strip_prefix("file:///")
            .or_else(|| url.strip_prefix("file://"))
            .unwrap_or(&url);

        // On Unix, ensure leading slash
        let result = if !path.starts_with('/') {
            format!("/{path}")
        } else {
            path.to_string()
        };

        Ok(Value::string(JsString::new_gc(&result)))
    }
}
