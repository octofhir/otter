//! `node:string_decoder` / `string_decoder` hosted module — boundary-safe
//! Buffer-to-string decoding, implemented as a JS shim over `buffer`.

use otter_runtime::CapabilitySet;
use otter_vm::{NativeCtx, Value};

const SHIM: &str = include_str!("string_decoder.js");

/// CommonJS export: the `string_decoder` namespace (`StringDecoder`).
pub fn string_decoder_cjs_value(
    ctx: &mut NativeCtx<'_>,
    caps: &CapabilitySet,
) -> Result<Value, String> {
    let buffer = crate::buffer::buffer_cjs_value(ctx, caps)?;
    otter_runtime::run_builtin_cjs_shim(ctx, "node:string_decoder", SHIM, &[("buffer", buffer)])
}

/// ESM namespace install — CommonJS is the supported surface for now.
pub fn install_string_decoder_module(
    _ctx: &mut otter_runtime::HostedModuleCtx<'_>,
) -> Result<(), String> {
    Ok(())
}
