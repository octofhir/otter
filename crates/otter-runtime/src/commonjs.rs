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
//! - `cjs_load` - resolve builtins, files, `node_modules` packages, and native
//!   addons, then cache and load their exports.
//!
//! # Invariants
//! - The require cache is a plain JS object (`require.cache`), so cached
//!   `module.exports` values stay rooted by GC. A module is inserted into the
//!   cache *before* its body runs so circular `require` returns the partial
//!   exports (Node behaviour).
//! - Cache handles enter a [`NativeCtx`] handle scope before hosted-module
//!   installers run because an installer may trigger a moving collection
//!   before its exports are stored.
//! - Filesystem capabilities are checked before any module or package manifest
//!   is read; native addons additionally pass through the configured loader's
//!   FFI capability check.
//! - Re-entry uses [`otter_vm::Interpreter::run_callable_sync`] and the
//!   code-space-linked wrapper from `create_commonjs_wrapper`; the unsafe
//!   `Interpreter::run` (which swaps `code_space`) is never called nested.
//!
//! # See also
//! - [`crate::CommonJsAddonLoader`]
//! - [`crate::RuntimeBuilder::commonjs_addon_loader`]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use smallvec::{SmallVec, smallvec};

use otter_vm::{NativeCtx, Value, object};

use crate::{
    CapabilitySet, CommonJsAddonLoader, HostedModule, RuntimeNativeError as NativeError,
    RuntimeTaskSpawner, runtime_string_value, runtime_type_error,
};

/// Shared configuration for a CommonJS run: capability snapshot, hosted
/// modules, task spawner, and optional native-addon loader.
#[derive(Clone)]
pub(crate) struct CjsConfig {
    pub(crate) capabilities: CapabilitySet,
    pub(crate) hosted: Vec<HostedModule>,
    pub(crate) runtime_task_spawner: Option<RuntimeTaskSpawner>,
    pub(crate) addon_loader: Option<CommonJsAddonLoader>,
}

fn oom(err: impl std::fmt::Display) -> NativeError {
    runtime_type_error("require", format!("out of memory: {err}"))
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
    // Allocate `module` / `exports`, then IMMEDIATELY root them on the
    // GC-traced module-root stack — before any further allocation. A collection
    // landing in an unrooted window (e.g. under `OTTER_GC_STRESS=full`) would
    // otherwise reclaim these young objects and reuse their offsets, leaving the
    // bare locals dangling. The module-root stack relocates its slots in place,
    // so after every subsequent allocation we re-fetch the live handles from the
    // slots instead of trusting the stale `module` / `exports` locals.
    let module_val = ctx
        .scope(|mut scope| {
            let exports = scope.bare_object()?;
            let module = scope.bare_object()?;
            scope.set(module, "exports", exports)?;
            Ok::<_, NativeError>(scope.finish(module))
        })
        .map_err(|err| err.to_string())?;

    let root_base = ctx.interp_mut().module_root_depth();
    let module_idx = ctx.interp_mut().push_module_root(module_val) - 1;
    let module = ctx
        .interp_mut()
        .module_root(module_idx)
        .as_object()
        .expect("module object survives module-root rooting");
    let exports_val =
        object::get(module, ctx.heap(), "exports").expect("module owns rooted exports");
    let exports_idx = ctx.interp_mut().push_module_root(exports_val) - 1;

    let id_val = runtime_string_value(ctx, name).map_err(|e| e.to_string())?;
    let id_idx = ctx.interp_mut().push_module_root(id_val) - 1;
    let mut module = ctx
        .interp_mut()
        .module_root(module_idx)
        .as_object()
        .expect("module object survives module-root rooting");
    let id_val = ctx.interp_mut().module_root(id_idx);
    object::set(&mut module, ctx.heap_mut(), "id", id_val);
    object::set(&mut module, ctx.heap_mut(), "loaded", Value::boolean(false));

    let name_val = runtime_string_value(ctx, name).map_err(|e| e.to_string())?;
    let name_idx = ctx.interp_mut().push_module_root(name_val) - 1;
    let require_val = make_shim_require(ctx, deps)?;
    let require_idx = ctx.interp_mut().push_module_root(require_val) - 1;

    // Re-fetch the relocated handles after the require/string allocations.
    let module_val = ctx.interp_mut().module_root(module_idx);
    let exports_val = ctx.interp_mut().module_root(exports_idx);

    let wrapper = ctx
        .create_commonjs_wrapper(name, source)
        .map_err(|e| e.to_string())?;
    let wrapper_idx = ctx.interp_mut().push_module_root(wrapper) - 1;
    let wrapper = ctx.interp_mut().module_root(wrapper_idx);
    let require_val = ctx.interp_mut().module_root(require_idx);
    let name_val = ctx.interp_mut().module_root(name_idx);
    let call_args: SmallVec<[Value; 8]> =
        smallvec![exports_val, require_val, module_val, name_val, name_val,];

    let run = ctx
        .call(wrapper, exports_val, call_args.as_slice())
        .map_err(|error| error.to_string());

    let module = ctx
        .interp_mut()
        .module_root(module_idx)
        .as_object()
        .expect("module object survives module-root rooting");
    let exports_val = ctx.interp_mut().module_root(exports_idx);

    let result = run.map(|_ret| object::get(module, ctx.heap(), "exports").unwrap_or(exports_val));

    ctx.interp_mut().pop_module_roots_to(root_base);
    result
}

/// Build a `require` for a shim that resolves only the supplied dependencies.
/// The deps are stored on a plain JS object (which roots their values) captured
/// by the closure; `require(spec)` returns `deps[spec]` or throws.
fn make_shim_require(ctx: &mut NativeCtx<'_>, deps: &[(&str, Value)]) -> Result<Value, String> {
    let table = ctx.scope(|mut scope| {
        let deps: Vec<_> = deps
            .iter()
            .map(|(spec, value)| (*spec, scope.value(*value)))
            .collect();
        let table = scope.bare_object().map_err(|err| err.to_string())?;
        for (spec, value) in deps {
            scope
                .set(table, spec, value)
                .map_err(|err| err.to_string())?;
        }
        Ok::<Value, String>(scope.finish(table))
    })?;
    let captures: SmallVec<[Value; 4]> = smallvec![table];
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
    ctx.native_value("require", captures, closure)
        .map_err(|err| err.to_string())
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

fn canonical_builtin_cache_key(cfg: &CjsConfig, spec: &str) -> String {
    if spec.starts_with("node:") {
        return spec.to_string();
    }
    let prefixed = format!("node:{spec}");
    if cfg.hosted.iter().any(|h| h.specifier() == prefixed) {
        prefixed
    } else {
        spec.to_string()
    }
}

/// Resolve a file/directory candidate using Node's CommonJS extension and
/// package-main probes.
fn resolve_path(base: &Path, capabilities: &CapabilitySet) -> Option<PathBuf> {
    let candidates = [
        base.to_path_buf(),
        base.with_extension("js"),
        base.with_extension("cjs"),
        base.with_extension("json"),
        base.with_extension("node"),
    ];
    for candidate in candidates {
        if candidate.is_file() {
            return std::fs::canonicalize(&candidate).ok().or(Some(candidate));
        }
    }
    if base.is_dir() {
        let package_json = base.join("package.json");
        if capabilities.read.matches_path(&package_json)
            && let Ok(source) = std::fs::read_to_string(&package_json)
            && let Ok(package) = serde_json::from_str::<serde_json::Value>(&source)
            && let Some(main) = package.get("main").and_then(|value| value.as_str())
            && let Some(resolved) = resolve_path(&base.join(main), capabilities)
        {
            return Some(resolved);
        }
        for index in ["index.js", "index.cjs", "index.json", "index.node"] {
            let candidate = base.join(index);
            if candidate.is_file() {
                return std::fs::canonicalize(&candidate).ok().or(Some(candidate));
            }
        }
    }
    None
}

/// Resolve relative, absolute, and bare package specifiers. Bare packages walk
/// ancestor `node_modules` directories, including scoped package names and
/// subpaths (`@scope/pkg/subpath`).
fn resolve_file(dir: &Path, spec: &str, capabilities: &CapabilitySet) -> Option<PathBuf> {
    if Path::new(spec).is_absolute() {
        return resolve_path(Path::new(spec), capabilities);
    }
    if spec.starts_with('.') {
        return resolve_path(&dir.join(spec), capabilities);
    }
    for ancestor in dir.ancestors() {
        let candidate = ancestor.join("node_modules").join(spec);
        if let Some(resolved) = resolve_path(&candidate, capabilities) {
            return Some(resolved);
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
    // Root-aware native allocation (see `make_shim_require`): the closure
    // allocation traces the live RuntimeState roots — including the module-root
    // stack — rather than `no_roots`.
    ctx.native_value("require", captures, closure).map_err(oom)
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
        let key = canonical_builtin_cache_key(cfg, hm.specifier());
        if let Some(cached) = object::get(cache, ctx.heap(), &key) {
            return Ok(cached);
        }
        let root_base = ctx.interp_mut().module_root_depth();
        let cache_idx = ctx.interp_mut().push_module_root(Value::object(cache)) - 1;
        let result: Result<Value, NativeError> = (|| {
            let value = if let Some(value_install) = hm.cjs_value() {
                value_install(ctx, &cfg.capabilities)
                    .map_err(|err| runtime_type_error("require", err))?
            } else {
                // Shared with the ESM loader: one namespace object (and one run
                // of the installer's side effects) per builtin specifier per
                // isolate, whichever loader touches it first.
                let interp = ctx.interp_mut();
                let namespace = match interp.host_module_env_cached(hm.specifier()) {
                    Some(env) => env,
                    None => {
                        let env = hm
                            .install(interp, &cfg.capabilities, cfg.runtime_task_spawner.clone())
                            .map_err(|err| runtime_type_error("require", err))?;
                        interp.cache_host_module_env(Arc::from(hm.specifier()), env);
                        env
                    }
                };
                Value::object(namespace)
            };
            let cache = ctx
                .interp_mut()
                .module_root(cache_idx)
                .as_object()
                .expect("require cache survives hosted module installation");
            ctx.scope(|mut scope| {
                let cache = scope.value(Value::object(cache));
                let value = scope.value(value);
                scope.set(cache, &key, value)?;
                Ok(scope.finish(value))
            })
        })();
        ctx.interp_mut().pop_module_roots_to(root_base);
        return result;
    }

    // 2. File module.
    let resolved = resolve_file(dir, spec, &cfg.capabilities)
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
    if resolved.extension().is_some_and(|ext| ext == "node") {
        let loader = cfg.addon_loader.ok_or_else(|| {
            runtime_type_error(
                "require",
                format!("native addons are not enabled for '{id}'"),
            )
        })?;
        let root_base = ctx.interp_mut().module_root_depth();
        let cache_idx = ctx.interp_mut().push_module_root(Value::object(cache)) - 1;
        let result: Result<Value, NativeError> = (|| {
            let value = loader(
                ctx,
                &resolved,
                &cfg.capabilities,
                cfg.runtime_task_spawner.clone(),
            )?;
            let cache = ctx
                .interp_mut()
                .module_root(cache_idx)
                .as_object()
                .expect("require cache survives addon registration");
            ctx.scope(|mut scope| {
                let cache = scope.value(Value::object(cache));
                let value = scope.value(value);
                scope.set(cache, &id, value)?;
                Ok(scope.finish(value))
            })
        })();
        ctx.interp_mut().pop_module_roots_to(root_base);
        return result;
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

    // Build `module` + `exports` through handle scopes, then IMMEDIATELY root
    // each escaped value on the GC-traced module-root stack before the next
    // allocation. A collection landing between the two object allocations
    // would otherwise reclaim `exports` before it reached the module-root
    // stack. The stack relocates its slots in place, so after every subsequent
    // allocation we re-fetch the live handle instead of trusting a stale local.
    // Root the caller's `cache` handle BEFORE allocating `exports` / `module`:
    // those allocations can scavenge and relocate `cache` (a young object), and
    // a bare local pushed afterwards would already be a stale, moved-from handle.
    let root_base = ctx.interp_mut().module_root_depth();
    let cache_idx = ctx.interp_mut().push_module_root(Value::object(cache)) - 1;

    let module_val = ctx.scope(|mut scope| {
        let exports = scope.object()?;
        let module = scope.object()?;
        scope.set(module, "exports", exports)?;
        Ok::<Value, NativeError>(scope.finish(module))
    })?;
    let module_idx = ctx.interp_mut().push_module_root(module_val) - 1;
    let module = ctx
        .interp_mut()
        .module_root(module_idx)
        .as_object()
        .expect("module object survives module-root rooting");
    let exports_val = object::get(module, ctx.heap(), "exports")
        .expect("module owns rooted exports after construction");
    let exports_idx = ctx.interp_mut().push_module_root(exports_val) - 1;

    let id_val = runtime_string_value(ctx, &id)?;
    let mut module = ctx
        .interp_mut()
        .module_root(module_idx)
        .as_object()
        .expect("module object survives module-root rooting");
    object::set(&mut module, ctx.heap_mut(), "id", id_val);

    let filename_val = runtime_string_value(ctx, &id)?;
    let mut module = ctx
        .interp_mut()
        .module_root(module_idx)
        .as_object()
        .expect("module object survives module-root rooting");
    object::set(&mut module, ctx.heap_mut(), "filename", filename_val);
    object::set(&mut module, ctx.heap_mut(), "loaded", Value::boolean(false));

    // Circular-require guard: cache the partial exports before running.
    let mut cache = ctx
        .interp_mut()
        .module_root(cache_idx)
        .as_object()
        .expect("require cache survives module-root rooting");
    let exports_val = ctx.interp_mut().module_root(exports_idx);
    object::set(&mut cache, ctx.heap_mut(), &id, exports_val);

    // Per-module bindings. Each of `require` / `dirname` / `filename` is a young
    // handle that the *following* allocations here (and `create_commonjs_wrapper`
    // below) can scavenge and relocate, so park each on the module-root stack the
    // moment it exists and re-fetch the live handles just before the call. The
    // `cache` local is likewise stale after the `object::set` above — re-fetch it
    // before `make_require` captures it into the closure.
    let cache = ctx
        .interp_mut()
        .module_root(cache_idx)
        .as_object()
        .expect("require cache survives module-root rooting");
    let require_val = make_require(ctx, cfg.clone(), cache, dir.clone())?;
    let require_idx = ctx.interp_mut().push_module_root(require_val) - 1;
    let dirname_val = runtime_string_value(ctx, &dir.to_string_lossy())?;
    let dirname_idx = ctx.interp_mut().push_module_root(dirname_val) - 1;
    let filename_arg = runtime_string_value(ctx, &id)?;
    let filename_idx = ctx.interp_mut().push_module_root(filename_arg) - 1;

    // Compile the wrapper and run it.
    let wrapper = ctx.create_commonjs_wrapper(&id, source)?;
    let wrapper_idx = ctx.interp_mut().push_module_root(wrapper) - 1;
    let wrapper = ctx.interp_mut().module_root(wrapper_idx);
    // Re-fetch every argument from its module-root slot: `create_commonjs_wrapper`
    // (and the string/require allocations before it) may have relocated them.
    let exports_val = ctx.interp_mut().module_root(exports_idx);
    let require_val = ctx.interp_mut().module_root(require_idx);
    let module_val = ctx.interp_mut().module_root(module_idx);
    let filename_arg = ctx.interp_mut().module_root(filename_idx);
    let dirname_val = ctx.interp_mut().module_root(dirname_idx);
    let call_args: SmallVec<[Value; 8]> = smallvec![
        exports_val,
        require_val,
        module_val,
        filename_arg,
        dirname_val,
    ];
    // `run_callable_sync` evaluates the whole module body, which can trigger any
    // number of moving collections. `module` / `cache` / `exports` are already
    // parked on the GC-traced module-root stack (rooted right after allocation
    // above); the collector rewrites the slots in place, so we read the relocated
    // handles back afterwards. Without this, a module whose body allocates enough
    // to scavenge would leave these handles dangling and the post-run
    // `module.exports` read would dereference a forwarded (moved) object.
    let run = ctx.call(wrapper, exports_val, call_args.as_slice());

    // Relocated handles (the collector may have moved them during the run).
    let mut module = ctx
        .interp_mut()
        .module_root(module_idx)
        .as_object()
        .expect("module object survives module-root rooting");
    let mut cache = ctx
        .interp_mut()
        .module_root(cache_idx)
        .as_object()
        .expect("require cache survives module-root rooting");
    let exports_val = ctx.interp_mut().module_root(exports_idx);

    let result = run.map(|_ret| {
        // `module.exports` may have been reassigned by the module body.
        let final_exports = object::get(module, ctx.heap(), "exports").unwrap_or(exports_val);
        object::set(&mut module, ctx.heap_mut(), "loaded", Value::boolean(true));
        object::set(&mut cache, ctx.heap_mut(), &id, final_exports);
        final_exports
    });

    ctx.interp_mut().pop_module_roots_to(root_base);
    result
}
