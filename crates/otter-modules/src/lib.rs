mod ffi;
mod kv;
mod sql;

pub use ffi::{FFIType, FfiError, FfiResult, FfiSignature};
pub use kv::{KvError, KvResult, KvStore};
pub use sql::{SqlDatabase, SqlError, SqlResult};

use std::sync::Arc;

use otter_runtime::{HostedExtension, HostedExtensionModule, RuntimeProfile, RuntimeState};

#[derive(Debug, Default, Clone, Copy)]
pub struct OtterModulesExtension;

impl HostedExtension for OtterModulesExtension {
    fn name(&self) -> &str {
        "otter-modules"
    }

    fn profiles(&self) -> &[RuntimeProfile] {
        &[RuntimeProfile::Core]
    }

    fn install(&self, _runtime: &mut RuntimeState) -> Result<(), String> {
        Ok(())
    }

    fn native_modules(&self) -> Vec<HostedExtensionModule> {
        vec![
            HostedExtensionModule {
                specifier: "otter:kv".to_string(),
                loader: Arc::new(kv::KvModule),
            },
            HostedExtensionModule {
                specifier: "otter:ffi".to_string(),
                loader: Arc::new(ffi::FfiModule),
            },
            HostedExtensionModule {
                specifier: "otter:sql".to_string(),
                loader: Arc::new(sql::SqlModule),
            },
        ]
    }
}

#[must_use]
pub fn modules_extension() -> OtterModulesExtension {
    OtterModulesExtension
}
