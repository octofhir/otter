mod ffi;
mod kv;
mod sql;

pub use ffi::{FFIType, FfiError, FfiResult, FfiSignature};
pub use kv::{KvError, KvResult, KvStore};
pub use sql::{SqlDatabase, SqlError, SqlResult};

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
        let mut modules = kv::kv_module_entries();
        modules.extend(ffi::ffi_module_entries());
        modules.extend(sql::sql_module_entries());
        modules
    }
}

#[must_use]
pub fn modules_extension() -> OtterModulesExtension {
    OtterModulesExtension
}
