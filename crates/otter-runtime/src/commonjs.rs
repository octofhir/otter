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
//! - A file module or value-style hosted module is built in one
//!   [`NativeScope`] arena. Its cache, module record, exports, dependency
//!   values, require closure, and wrapper stay collector-rewritten until the
//!   result is published, and the arena is released on every exit.
//! - Hosted namespace and CommonJS-value installers run directly in the
//!   loader's existing handle scope. Namespace cache publication and
//!   `require.cache` publication happen before that scope closes.
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

use otter_vm::{Local, NativeCtx, NativeScope, Value, object};

use crate::{
    CapabilitySet, CommonJsAddonLoader, HostedModule, RuntimeNativeError as NativeError,
    RuntimeTaskSpawner, runtime_type_error,
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
/// Returns a native error on allocation, compile, or runtime failure.
pub fn run_builtin_cjs_shim<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    name: &str,
    source: &str,
    deps: &[(&str, Local<'_>)],
) -> Result<Local<'scope>, NativeError> {
    let exports = scope.bare_object()?;
    let module = scope.bare_object()?;
    scope.set(module, "exports", exports)?;

    let id = scope.string(name)?;
    scope.set(module, "id", id)?;
    let loaded = scope.boolean(false);
    scope.set(module, "loaded", loaded)?;

    let require = make_shim_require(scope, deps)?;
    let module_name = scope.string(name)?;
    let wrapper = scope.commonjs_wrapper(name, source)?;
    scope.call(
        wrapper,
        exports,
        &[exports, require, module, module_name, module_name],
    )?;

    let loaded = scope.boolean(true);
    scope.set(module, "loaded", loaded)?;
    scope.get(module, "exports")
}

/// Build a `require` for a shim that resolves only the supplied dependencies.
/// The deps are stored on a plain JS object (which roots their values) captured
/// by the closure; `require(spec)` returns `deps[spec]` or throws.
fn make_shim_require<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    deps: &[(&str, Local<'_>)],
) -> Result<Local<'scope>, NativeError> {
    let table = scope.bare_object()?;
    for (specifier, value) in deps {
        scope.set(table, specifier, *value)?;
    }
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
    scope.native_closure("require", 1, &[table], closure)
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
fn make_require<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    cfg: Arc<CjsConfig>,
    cache: Local<'_>,
    dir: PathBuf,
) -> Result<Local<'scope>, NativeError> {
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
    scope.native_closure("require", 1, &[cache], closure)
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
        return ctx.scope(|mut scope| {
            let cache = scope.value(Value::object(cache));
            let value = if hm.shares_namespace_with_commonjs() {
                match scope.cached_host_module_env(hm.specifier()) {
                    Some(namespace) => namespace,
                    None => {
                        let namespace = (hm.commonjs_install())(
                            &mut scope,
                            &cfg.capabilities,
                            cfg.runtime_task_spawner.clone(),
                        )?;
                        scope.cache_host_module_env(hm.specifier(), namespace)?;
                        namespace
                    }
                }
            } else {
                (hm.commonjs_install())(
                    &mut scope,
                    &cfg.capabilities,
                    cfg.runtime_task_spawner.clone(),
                )?
            };
            scope.set(cache, &key, value)?;
            Ok(scope.finish(value))
        });
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
        let result = (|| {
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

    ctx.scope(|mut scope| {
        let cache = scope.value(Value::object(cache));
        let exports = scope.object()?;
        let module = scope.object()?;
        scope.set(module, "exports", exports)?;
        let id_value = scope.string(&id)?;
        scope.set(module, "id", id_value)?;
        scope.set(module, "filename", id_value)?;
        let loaded = scope.boolean(false);
        scope.set(module, "loaded", loaded)?;

        // Circular-require guard: cache the partial exports before running.
        scope.set(cache, &id, exports)?;

        let require = make_require(&mut scope, cfg.clone(), cache, dir.clone())?;
        let dirname = scope.string(&dir.to_string_lossy())?;
        let wrapper = scope.commonjs_wrapper(&id, source)?;
        scope.call(
            wrapper,
            exports,
            &[exports, require, module, id_value, dirname],
        )?;

        // `module.exports` may have been reassigned by the module body.
        let final_exports = scope.get(module, "exports")?;
        let loaded = scope.boolean(true);
        scope.set(module, "loaded", loaded)?;
        scope.set(cache, &id, final_exports)?;
        Ok(scope.finish(final_exports))
    })
}
