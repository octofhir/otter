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

/// Propagate a VM error across the `require` boundary *intact* — a thrown JS
/// value stays a thrown value (no re-wrapping/stringifying at each nested
/// require level), so the original error survives to the outermost boundary.
fn vm_err(err: otter_vm::VmError) -> NativeError {
    otter_vm::native_function::vm_to_native_error(err, "require")
}

/// Run an embedded JavaScript shim as a CommonJS module and return its
/// `module.exports`. For builtin modules whose natural implementation is a
/// self-contained JS class or helper set (e.g. `events`, `node:test`).
///
/// The shim runs through the same wrapper as a file module
/// (`(function (exports, require, module, __filename, __dirname) { ... })`).
/// `require` resolves only the explicitly supplied `deps` (name → value);
/// anything else throws `Cannot find module`. `__filename`/`__dirname` are the
/// module name (diagnostics only). Pass `&[]` for a dependency-free shim.
///
/// # Errors
/// Returns a string on compile or runtime failure of the shim.
pub fn run_builtin_cjs_shim(
    ctx: &mut NativeCtx<'_>,
    name: &str,
    source: &str,
    deps: &[(&str, Value)],
) -> Result<Value, String> {
    let exports = ctx.alloc_object().map_err(|e| e.to_string())?;
    let module = ctx.alloc_object().map_err(|e| e.to_string())?;
    let exports_val = Value::object(exports);
    object::set(module, ctx.heap_mut(), "exports", exports_val);
    let id_val = runtime_string_value(ctx, name).map_err(|e| e.to_string())?;
    object::set(module, ctx.heap_mut(), "id", id_val);
    object::set(module, ctx.heap_mut(), "loaded", Value::boolean(false));
    let module_val = Value::object(module);
    let name_val = runtime_string_value(ctx, name).map_err(|e| e.to_string())?;
    let require_val = make_shim_require(ctx, deps)?;

    let (interp, context) = ctx.interp_mut_and_context();
    let context = context.ok_or_else(|| "missing execution context for shim load".to_string())?;
    let wrapper = interp
        .create_commonjs_wrapper(source)
        .map_err(|e| e.to_string())?;
    let call_args: SmallVec<[Value; 8]> =
        smallvec![exports_val, require_val, module_val, name_val, name_val,];

    // See `cjs_instantiate_file`: park `module` / `exports` on the GC-traced
    // module-root stack so a collection during the shim body does not leave
    // these young-object handles dangling.
    let root_base = interp.module_root_depth();
    let module_idx = interp.push_module_root(module_val) - 1;
    let exports_idx = interp.push_module_root(exports_val) - 1;

    let run = interp.run_callable_sync(&context, &wrapper, exports_val, call_args);

    let module = interp
        .module_root(module_idx)
        .as_object()
        .expect("module object survives module-root rooting");
    let exports_val = interp.module_root(exports_idx);

    let result = run
        .map_err(|e| e.to_string())
        .map(|_ret| object::get(module, interp.gc_heap(), "exports").unwrap_or(exports_val));

    interp.pop_module_roots_to(root_base);
    result
}

/// Build a `require` for a shim that resolves only the supplied dependencies.
/// The deps are stored on a plain JS object (which roots their values) captured
/// by the closure; `require(spec)` returns `deps[spec]` or throws.
fn make_shim_require(ctx: &mut NativeCtx<'_>, deps: &[(&str, Value)]) -> Result<Value, String> {
    let table = ctx.alloc_object().map_err(|e| e.to_string())?;
    for (spec, value) in deps {
        object::set(table, ctx.heap_mut(), spec, *value);
    }
    let captures: SmallVec<[Value; 4]> = smallvec![Value::object(table)];
    let closure = move |ctx: &mut NativeCtx<'_>,
                        args: &[Value],
                        captures: &[Value]|
          -> Result<Value, NativeError> {
        let table = captures
            .first()
            .and_then(|value| value.as_object())
            .ok_or_else(|| runtime_type_error("require", "missing shim dependency table"))?;
        let spec = crate::runtime_arg_to_string(args, 0, ctx.heap());
        match object::get(table, ctx.heap(), &spec) {
            Some(value) => Ok(value),
            None => Err(runtime_type_error(
                "require",
                format!("Cannot find module '{spec}'"),
            )),
        }
    };
    otter_vm::native_value_with_captures(ctx.heap_mut(), "require", captures, closure)
        .map_err(|e| e.to_string())
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
        let value = if let Some(value_install) = hm.cjs_value() {
            // Value installers (e.g. `assert`) build via `ModuleScope`, which
            // pops its roots on return — so root the export across the cache
            // store, which itself may allocate.
            value_install(ctx, &cfg.capabilities)
                .map_err(|err| runtime_type_error("require", err))?
        } else {
            let interp = ctx.interp_mut();
            let namespace = hm
                .install(interp, &cfg.capabilities)
                .map_err(|err| runtime_type_error("require", err))?;
            Value::object(namespace)
        };
        let depth = ctx.interp_mut().push_module_root(value);
        let value = ctx.interp_mut().module_root(depth - 1);
        object::set(cache, ctx.heap_mut(), key, value);
        let value = object::get(cache, ctx.heap(), key).unwrap_or(value);
        ctx.interp_mut().pop_module_roots_to(depth - 1);
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
    // `run_callable_sync` evaluates the whole module body, which can trigger any
    // number of moving collections. `module` / `cache` / `exports` are young
    // objects held only in these Rust locals, so park them on the GC-traced
    // module-root stack (the collector rewrites the slots in place) and read the
    // relocated handles back afterwards. Without this, a module whose body
    // allocates enough to scavenge leaves these handles dangling and the
    // post-run `module.exports` read dereferences a forwarded (moved) object.
    let root_base = interp.module_root_depth();
    let module_idx = interp.push_module_root(module_val) - 1;
    let cache_idx = interp.push_module_root(Value::object(cache)) - 1;
    let exports_idx = interp.push_module_root(exports_val) - 1;

    eprintln!(
        "[DBG file] BEFORE run: root_base={root_base} module_idx={module_idx} depth={} module_bits={:#018x} as_obj_some={} stress_armed={} stride={} extra_roots={} frame_providers={}",
        interp.module_root_depth(),
        interp.module_root(module_idx).to_bits(),
        interp.module_root(module_idx).as_object().is_some(),
        interp.gc_heap().dbg_stress_armed(),
        interp.gc_heap().dbg_stress_stride(),
        interp.gc_heap().dbg_extra_roots_len(),
        interp.gc_heap().dbg_frame_providers_len(),
    );

    let run = interp.run_callable_sync(&context, &wrapper, exports_val, call_args);

    eprintln!(
        "[DBG file] AFTER run: depth={} module_bits={:#018x} as_obj_some={} extra_roots={} frame_providers={}",
        interp.module_root_depth(),
        interp.module_root(module_idx).to_bits(),
        interp.module_root(module_idx).as_object().is_some(),
        interp.gc_heap().dbg_extra_roots_len(),
        interp.gc_heap().dbg_frame_providers_len(),
    );

    // Relocated handles (the collector may have moved them during the run).
    let module = interp
        .module_root(module_idx)
        .as_object()
        .expect("module object survives module-root rooting");
    let cache = interp
        .module_root(cache_idx)
        .as_object()
        .expect("require cache survives module-root rooting");
    let exports_val = interp.module_root(exports_idx);

    let result = run.map_err(vm_err).map(|_ret| {
        // `module.exports` may have been reassigned by the module body.
        let final_exports = object::get(module, interp.gc_heap(), "exports").unwrap_or(exports_val);
        object::set(module, interp.gc_heap_mut(), "loaded", Value::boolean(true));
        object::set(cache, interp.gc_heap_mut(), &id, final_exports);
        final_exports
    });

    interp.pop_module_roots_to(root_base);
    result
}
