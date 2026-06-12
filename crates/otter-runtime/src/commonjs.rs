//! CommonJS module system for the active runtime.
//!
//! Implements Node's `require` / `module.exports` / `__dirname` semantics on top
//! of the VM. Each CommonJS file runs inside a wrapper function
//! `(function (exports, require, module, __filename, __dirname) { ... })` built
//! by [`otter_vm::Interpreter::create_commonjs_wrapper`], and `require` is a
//! per-module native closure that re-enters the runtime synchronously to load
//! dependencies.
//!
//! # Contents
//! - [`CjsConfig`] - capability snapshot + hosted-module list shared by all
//!   `require` closures in a run.
//! - [`cjs_instantiate_file`] - compile + execute one CommonJS file.
//! - `cjs_load` - resolve a specifier (builtin or relative file) and load it.
//!
//! # Invariants
//! - The require cache is a plain JS object (`require.cache`), so cached
//!   `module.exports` values stay rooted by GC. A module is inserted into the
//!   cache *before* its body runs so circular `require` returns the partial
//!   exports (Node behaviour).
//! - Filesystem capabilities are checked before any module file is read.
//! - Re-entry uses [`otter_vm::Interpreter::run_callable_sync`] and the
//!   code-space-linked wrapper from `create_commonjs_wrapper`; the unsafe
//!   `Interpreter::run` (which swaps `code_space`) is never called nested.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use smallvec::{SmallVec, smallvec};

use otter_vm::{NativeCtx, Value, object};

use crate::{
    CapabilitySet, HostedModule, RuntimeNativeError as NativeError, runtime_string_value,
    runtime_type_error,
};

/// Shared configuration for a CommonJS run: capability snapshot and the hosted
/// (builtin) module list used to satisfy `require('node:fs')` and friends.
#[derive(Clone)]
pub(crate) struct CjsConfig {
    pub(crate) capabilities: CapabilitySet,
    pub(crate) hosted: Vec<HostedModule>,
}

fn oom(err: impl std::fmt::Display) -> NativeError {
    runtime_type_error("require", format!("out of memory: {err}"))
}

fn vm_err(err: otter_vm::VmError) -> NativeError {
    runtime_type_error("require", format!("{err:?}"))
}

/// Resolve a builtin (hosted) module by specifier. Matches the bare specifier
/// directly (`fs`) or the `node:`-prefixed form (`node:fs`).
fn resolve_builtin(cfg: &CjsConfig, spec: &str) -> Option<HostedModule> {
    if let Some(hm) = cfg.hosted.iter().find(|h| h.specifier() == spec) {
        return Some(*hm);
    }
    if !spec.starts_with('.') && !spec.contains('/') {
        let prefixed = format!("node:{spec}");
        if let Some(hm) = cfg.hosted.iter().find(|h| h.specifier() == prefixed) {
            return Some(*hm);
        }
    }
    None
}

/// Resolve a relative/absolute file specifier to a concrete file path, probing
/// the standard CommonJS extension + `index` candidates.
fn resolve_file(dir: &Path, spec: &str) -> Option<PathBuf> {
    let base = if Path::new(spec).is_absolute() {
        PathBuf::from(spec)
    } else {
        dir.join(spec)
    };
    let candidates = [
        base.clone(),
        base.with_extension("js"),
        base.with_extension("cjs"),
        base.with_extension("json"),
        base.join("index.js"),
        base.join("index.cjs"),
    ];
    for candidate in candidates {
        if candidate.is_file() {
            return std::fs::canonicalize(&candidate).ok().or(Some(candidate));
        }
    }
    None
}

/// Build a per-module `require` native function bound to `dir`. The shared cache
/// object is passed as a traced VM capture; the directory and config are moved
/// into the Rust closure.
fn make_require(
    ctx: &mut NativeCtx<'_>,
    cfg: Arc<CjsConfig>,
    cache: object::JsObject,
    dir: PathBuf,
) -> Result<Value, NativeError> {
    let captures: SmallVec<[Value; 4]> = smallvec![Value::object(cache)];
    let closure = move |ctx: &mut NativeCtx<'_>,
                        args: &[Value],
                        captures: &[Value]|
          -> Result<Value, NativeError> {
        let cache = captures
            .first()
            .and_then(|value| value.as_object())
            .ok_or_else(|| runtime_type_error("require", "missing require cache"))?;
        let spec = crate::runtime_arg_to_string(args, 0, ctx.heap());
        if spec.is_empty() {
            return Err(runtime_type_error(
                "require",
                "module specifier is required",
            ));
        }
        cjs_load(ctx, &cfg, cache, &dir, &spec)
    };
    otter_vm::native_value_with_captures(ctx.heap_mut(), "require", captures, closure).map_err(oom)
}

/// Resolve and load a module by specifier from `dir`. Returns the module's
/// exports value.
pub(crate) fn cjs_load(
    ctx: &mut NativeCtx<'_>,
    cfg: &Arc<CjsConfig>,
    cache: object::JsObject,
    dir: &Path,
    spec: &str,
) -> Result<Value, NativeError> {
    // 1. Builtin (hosted) module.
    if let Some(hm) = resolve_builtin(cfg, spec) {
        let key = hm.specifier();
        if let Some(cached) = object::get(cache, ctx.heap(), key) {
            return Ok(cached);
        }
        let namespace = {
            let interp = ctx.interp_mut();
            hm.install(interp, &cfg.capabilities)
                .map_err(|err| runtime_type_error("require", err))?
        };
        let value = Value::object(namespace);
        object::set(cache, ctx.heap_mut(), key, value);
        return Ok(value);
    }

    // 2. File module.
    let resolved = resolve_file(dir, spec)
        .ok_or_else(|| runtime_type_error("require", format!("Cannot find module '{spec}'")))?;
    let id = resolved.to_string_lossy().to_string();
    if let Some(cached) = object::get(cache, ctx.heap(), &id) {
        return Ok(cached);
    }
    if !cfg.capabilities.read.matches_path(&resolved) {
        return Err(runtime_type_error(
            "require",
            format!("permission denied for '{id}'"),
        ));
    }
    let source = std::fs::read_to_string(&resolved)
        .map_err(|err| runtime_type_error("require", format!("io error for '{id}': {err}")))?;
    cjs_instantiate_file(ctx, cfg, cache, &resolved, &source)
}

/// Compile and execute one CommonJS file, returning its `module.exports`.
pub(crate) fn cjs_instantiate_file(
    ctx: &mut NativeCtx<'_>,
    cfg: &Arc<CjsConfig>,
    cache: object::JsObject,
    abs: &Path,
    source: &str,
) -> Result<Value, NativeError> {
    let id = abs.to_string_lossy().to_string();
    let dir = abs
        .parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);

    // Build `module` + `exports`.
    let exports = ctx.alloc_object().map_err(oom)?;
    let module = ctx.alloc_object().map_err(oom)?;
    let exports_val = Value::object(exports);
    object::set(module, ctx.heap_mut(), "exports", exports_val);
    let id_val = runtime_string_value(ctx, &id)?;
    object::set(module, ctx.heap_mut(), "id", id_val);
    let filename_val = runtime_string_value(ctx, &id)?;
    object::set(module, ctx.heap_mut(), "filename", filename_val);
    object::set(module, ctx.heap_mut(), "loaded", Value::boolean(false));
    let module_val = Value::object(module);

    // Circular-require guard: cache the partial exports before running.
    object::set(cache, ctx.heap_mut(), &id, exports_val);

    // Per-module bindings.
    let require_val = make_require(ctx, cfg.clone(), cache, dir.clone())?;
    let dirname_val = runtime_string_value(ctx, &dir.to_string_lossy())?;
    let filename_arg = runtime_string_value(ctx, &id)?;

    // Compile the wrapper and run it.
    let (interp, context) = ctx.interp_mut_and_context();
    let context = context.ok_or_else(|| {
        runtime_type_error("require", "missing execution context for module load")
    })?;
    let wrapper = interp.create_commonjs_wrapper(source).map_err(vm_err)?;
    let call_args: SmallVec<[Value; 8]> = smallvec![
        exports_val,
        require_val,
        module_val,
        filename_arg,
        dirname_val,
    ];
    interp
        .run_callable_sync(&context, &wrapper, exports_val, call_args)
        .map_err(vm_err)?;

    // `module.exports` may have been reassigned by the module body.
    let final_exports = object::get(module, interp.gc_heap(), "exports").unwrap_or(exports_val);
    object::set(module, interp.gc_heap_mut(), "loaded", Value::boolean(true));
    object::set(cache, interp.gc_heap_mut(), &id, final_exports);
    Ok(final_exports)
}
