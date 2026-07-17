//! `node:string_decoder` / `string_decoder` hosted module — boundary-safe
//! Buffer-to-string decoding, implemented as a JS shim over `buffer`.

use otter_runtime::{CapabilitySet, RuntimeNativeError as NativeError, RuntimeTaskSpawner};
use otter_vm::{Local, NativeScope};

const SHIM: &str = include_str!("string_decoder.js");

/// CommonJS export: the `string_decoder` namespace (`StringDecoder`).
pub fn string_decoder_cjs_value<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    _caps: &CapabilitySet,
    _runtime_task_spawner: Option<RuntimeTaskSpawner>,
    module: Local<'scope>,
    require: Local<'scope>,
) -> Result<Local<'scope>, NativeError> {
    otter_runtime::run_builtin_cjs_shim(scope, "node:string_decoder", SHIM, module, require)
}
