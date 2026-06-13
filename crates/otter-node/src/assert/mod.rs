//! `node:assert` / `assert` hosted module.
//!
//! Assert is largely a JavaScript surface in Node (a real `AssertionError`
//! class, matcher validation, deep equality, `rejects`/`doesNotReject`,
//! `CallTracker`), so it ships as embedded JS run through
//! [`otter_runtime::run_builtin_cjs_shim`]. The implementation is split to
//! mirror Node's own layout (and to keep each file well under the
//! split-at-1000-lines threshold):
//!
//! # Contents
//! - [`assert_cjs_value`] - the callable `assert` namespace (`assert.js`),
//!   with `util` + the `internal/assert/calltracker` factory injected.
//! - [`myers_diff_cjs_value`] - `internal/assert/myers_diff` exposed as its own
//!   requirable module (the conformance suite imports it directly under
//!   `--expose-internals`).
//!
//! # See also
//! - `assert/assert.js`, `assert/calltracker.js`, `assert/myers_diff.js`.

use otter_runtime::CapabilitySet;
use otter_vm::{NativeCtx, Value};

/// Embedded `assert` surface.
const ASSERT_JS: &str = include_str!("assert.js");
/// Embedded `internal/assert/calltracker` factory.
const CALLTRACKER_JS: &str = include_str!("calltracker.js");
/// Embedded `internal/assert/myers_diff`.
const MYERS_DIFF_JS: &str = include_str!("myers_diff.js");

/// CommonJS export: the callable `assert` namespace.
pub fn assert_cjs_value(ctx: &mut NativeCtx<'_>, caps: &CapabilitySet) -> Result<Value, String> {
    let util = crate::util::util_cjs_value(ctx, caps)?;
    let calltracker = otter_runtime::run_builtin_cjs_shim(
        ctx,
        "internal/assert/calltracker",
        CALLTRACKER_JS,
        &[],
    )?;
    let myers =
        otter_runtime::run_builtin_cjs_shim(ctx, "internal/assert/myers_diff", MYERS_DIFF_JS, &[])?;
    otter_runtime::run_builtin_cjs_shim(
        ctx,
        "assert",
        ASSERT_JS,
        &[
            ("util", util),
            ("internal/assert/calltracker", calltracker),
            ("internal/assert/myers_diff", myers),
        ],
    )
}

/// CommonJS export for `internal/assert/myers_diff` (`--expose-internals`).
pub fn myers_diff_cjs_value(
    ctx: &mut NativeCtx<'_>,
    _caps: &CapabilitySet,
) -> Result<Value, String> {
    otter_runtime::run_builtin_cjs_shim(ctx, "internal/assert/myers_diff", MYERS_DIFF_JS, &[])
}

/// ESM namespace install — CommonJS is the supported surface for now.
pub fn install_assert_module(_ctx: &mut otter_runtime::HostedModuleCtx<'_>) -> Result<(), String> {
    Ok(())
}

/// ESM namespace install for the internal myers-diff module.
pub fn install_myers_diff_module(
    _ctx: &mut otter_runtime::HostedModuleCtx<'_>,
) -> Result<(), String> {
    Ok(())
}
