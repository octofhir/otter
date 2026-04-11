mod assert;
mod child_process;
mod fs;
mod module_registry;
mod net;
mod node_test;
mod path;
mod process;
mod support;
mod url;
mod util;
mod vm;
mod worker_threads;

use otter_runtime::{HostedExtension, HostedExtensionModule, RuntimeProfile, RuntimeState};

pub use module_registry::NodeModuleEntry;

#[derive(Debug, Default, Clone, Copy)]
pub struct OtterNodeJsExtension;

impl HostedExtension for OtterNodeJsExtension {
    fn name(&self) -> &str {
        "otter-nodejs"
    }

    fn profiles(&self) -> &[RuntimeProfile] {
        &[RuntimeProfile::Full]
    }

    fn install(&self, runtime: &mut RuntimeState) -> Result<(), String> {
        process::install_process_global(runtime)
    }

    fn native_modules(&self) -> Vec<HostedExtensionModule> {
        let mut entries = process::process_module_entries();
        entries.extend(assert::assert_module_entries());
        entries.extend(util::util_module_entries());
        entries.extend(worker_threads::worker_threads_module_entries());
        entries.extend(net::net_module_entries());
        entries.extend(path::path_module_entries());
        entries.extend(url::url_module_entries());
        entries.extend(node_test::node_test_module_entries());
        entries.extend(child_process::child_process_module_entries());
        entries.extend(vm::vm_module_entries());
        entries.extend(fs::fs_module_entries());
        entries
    }
}

#[must_use]
pub fn nodejs_extension() -> OtterNodeJsExtension {
    OtterNodeJsExtension
}

#[must_use]
pub fn builtin_modules() -> &'static [&'static str] {
    module_registry::builtin_modules()
}
