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
//! - [`cjs_instantiate_file`] - compile + execute one CommonJS entry file.
//! - `resolve_module` - canonical hosted/file resolution and cache keys.
//! - `cjs_load` - load hosted modules, files, and native addons through the
//!   shared module-record cache.
//!
//! # Invariants
//! - The require cache is one null-prototype JS object exposed as
//!   `require.cache`. Its values are live module records, never export
//!   snapshots. Every hit, including a circular back-edge, reads the record's
//!   current `module.exports`.
//! - A record is inserted with `loaded = false` before evaluation. Every
//!   abrupt completion removes that partial record before propagating the
//!   original error, so a later `require` retries installation.
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

use otter_vm::{Attr, Local, NativeCtx, NativeScope, Value, object};

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
/// The supplied `module` is the live record already present in the shared
/// cache and `require` is the importing module's canonical resolver. A shim's
/// dependency loads and `module.exports` replacements therefore participate in
/// exactly the same singleton and circular-loading semantics as file modules.
/// `__filename`/`__dirname` use `name` for diagnostics.
///
/// # Errors
/// Returns a native error on allocation, compile, or runtime failure.
pub fn run_builtin_cjs_shim<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    name: &str,
    source: &str,
    module: Local<'scope>,
    require: Local<'scope>,
) -> Result<Local<'scope>, NativeError> {
    let exports = scope.get(module, "exports")?;
    let module_name = scope.string(name)?;
    let wrapper = scope.commonjs_wrapper(name, source)?;
    scope.call(
        wrapper,
        exports,
        &[exports, require, module, module_name, module_name],
    )?;
    scope.get(module, "exports")
}

/// Resolve one hosted-installer dependency through the supplied CommonJS
/// `require` function.
///
/// This is the Rust-side counterpart of writing `require(specifier)` in an
/// embedded shim. It deliberately invokes the rooted JavaScript resolver
/// rather than calling another installer directly, so aliases, cycles,
/// rollback, and singleton identity all use the shared canonical cache.
pub fn require_commonjs_dependency<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    require: Local<'_>,
    specifier: &str,
) -> Result<Local<'scope>, NativeError> {
    let specifier = scope.string(specifier)?;
    let this_value = scope.undefined();
    scope.call(require, this_value, &[specifier])
}

/// Resolve a builtin (hosted) module by specifier. Matches the bare specifier
/// directly (`fs`) or the `node:`-prefixed form (`node:fs`).
fn resolve_builtin(cfg: &CjsConfig, spec: &str) -> Option<HostedModule> {
    if !spec.starts_with("node:") && !spec.starts_with('.') && !Path::new(spec).is_absolute() {
        let prefixed = format!("node:{spec}");
        if let Some(hm) = cfg.hosted.iter().find(|h| h.specifier() == prefixed) {
            return Some(*hm);
        }
    }
    cfg.hosted
        .iter()
        .find(|hosted| hosted.specifier() == spec)
        .copied()
}

#[derive(Debug)]
enum CjsTarget {
    Hosted(HostedModule),
    File(PathBuf),
}

#[derive(Debug)]
struct CjsResolution {
    key: String,
    filename: String,
    dir: PathBuf,
    target: CjsTarget,
}

impl CjsResolution {
    fn file(path: PathBuf) -> Self {
        let filename = path.to_string_lossy().into_owned();
        let dir = path
            .parent()
            .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
        Self {
            key: filename.clone(),
            filename,
            dir,
            target: CjsTarget::File(path),
        }
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

/// Resolve one specifier and derive its sole cache key.
///
/// The selected hosted row is authoritative for aliases, so both
/// `fs/promises` and `node:fs/promises` use `node:fs/promises` when that is the
/// registered specifier. Files use their canonical absolute path.
fn resolve_module(cfg: &CjsConfig, dir: &Path, spec: &str) -> Result<CjsResolution, NativeError> {
    if let Some(hosted) = resolve_builtin(cfg, spec) {
        let key = hosted.specifier().to_string();
        return Ok(CjsResolution {
            filename: key.clone(),
            key,
            dir: dir.to_path_buf(),
            target: CjsTarget::Hosted(hosted),
        });
    }
    let path = resolve_file(dir, spec, &cfg.capabilities)
        .ok_or_else(|| runtime_type_error("require", format!("Cannot find module '{spec}'")))?;
    Ok(CjsResolution::file(path))
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
    let require = scope.native_closure("require", 1, &[cache], closure)?;
    scope.define(require, "cache", cache, Attr::data().to_flags())?;
    Ok(require)
}

fn cached_exports<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    cache: Local<'_>,
    key: &str,
) -> Result<Option<Local<'scope>>, NativeError> {
    let module = scope.get(cache, key)?;
    if scope.is_undefined(module) {
        return Ok(None);
    }
    Ok(Some(scope.get(module, "exports")?))
}

struct ModuleRecord<'scope> {
    module: Local<'scope>,
    exports: Local<'scope>,
    id: Local<'scope>,
}

/// Allocate and publish a partial module record. Publication is deliberately
/// the final fallible operation so every later error belongs to the rollback
/// transaction in `load_resolved_scoped`.
fn begin_module_record<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    cache: Local<'_>,
    resolution: &CjsResolution,
) -> Result<ModuleRecord<'scope>, NativeError> {
    let exports = scope.object()?;
    let module = scope.object()?;
    scope.set(module, "exports", exports)?;
    let id = scope.string(&resolution.filename)?;
    scope.set(module, "id", id)?;
    scope.set(module, "filename", id)?;
    let loaded = scope.boolean(false);
    scope.set(module, "loaded", loaded)?;
    scope.set(cache, &resolution.key, module)?;
    Ok(ModuleRecord {
        module,
        exports,
        id,
    })
}

fn finish_module<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    module: Local<'_>,
    exports: Local<'_>,
) -> Result<Local<'scope>, NativeError> {
    scope.set(module, "exports", exports)?;
    let loaded = scope.boolean(true);
    scope.set(module, "loaded", loaded)?;
    scope.get(module, "exports")
}

fn load_resolved_scoped<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    cfg: &Arc<CjsConfig>,
    cache: Local<'_>,
    resolution: &CjsResolution,
    entry_source: Option<&str>,
) -> Result<Local<'scope>, NativeError> {
    if let Some(exports) = cached_exports(scope, cache, &resolution.key)? {
        return Ok(exports);
    }

    let record = begin_module_record(scope, cache, resolution)?;
    let result = (|| {
        let require = make_require(scope, cfg.clone(), cache, resolution.dir.clone())?;
        match &resolution.target {
            CjsTarget::Hosted(hosted) => {
                let mut publish_namespace = false;
                let exports = if let Some(install) = hosted.commonjs_value_install() {
                    let installed = install(
                        scope,
                        &cfg.capabilities,
                        cfg.runtime_task_spawner.clone(),
                        record.module,
                        require,
                    )?;
                    let current = scope.get(record.module, "exports")?;
                    if scope.strict_equals(current, record.exports) {
                        installed
                    } else {
                        current
                    }
                } else if let Some(namespace) = scope.cached_host_module_env(hosted.specifier()) {
                    namespace
                } else {
                    let install = hosted
                        .namespace_install()
                        .expect("hosted module has a CommonJS installer");
                    let namespace =
                        install(scope, &cfg.capabilities, cfg.runtime_task_spawner.clone())?;
                    publish_namespace = true;
                    namespace
                };
                let exports = finish_module(scope, record.module, exports)?;
                if publish_namespace {
                    scope.cache_host_module_env(hosted.specifier(), exports)?;
                }
                Ok(exports)
            }
            CjsTarget::File(path) => {
                if !cfg.capabilities.read.matches_path(path) {
                    return Err(runtime_type_error(
                        "require",
                        format!("permission denied for '{}'", resolution.filename),
                    ));
                }
                if path.extension().is_some_and(|ext| ext == "node") {
                    let loader = cfg.addon_loader.ok_or_else(|| {
                        runtime_type_error(
                            "require",
                            format!(
                                "native addons are not enabled for '{}'",
                                resolution.filename
                            ),
                        )
                    })?;
                    let exports = loader(
                        scope,
                        path,
                        &cfg.capabilities,
                        cfg.runtime_task_spawner.clone(),
                    )?;
                    return finish_module(scope, record.module, exports);
                }
                let owned_source;
                let source = if let Some(source) = entry_source {
                    source
                } else {
                    owned_source = std::fs::read_to_string(path).map_err(|err| {
                        runtime_type_error(
                            "require",
                            format!("io error for '{}': {err}", resolution.filename),
                        )
                    })?;
                    &owned_source
                };
                let dirname = scope.string(&resolution.dir.to_string_lossy())?;
                let wrapper = scope.commonjs_wrapper(&resolution.filename, source)?;
                scope.call(
                    wrapper,
                    record.exports,
                    &[record.exports, require, record.module, record.id, dirname],
                )?;
                let exports = scope.get(record.module, "exports")?;
                finish_module(scope, record.module, exports)
            }
        }
    })();

    match result {
        Ok(exports) => Ok(exports),
        Err(error) => {
            let _ = scope.delete_if_same(cache, &resolution.key, record.module);
            Err(error)
        }
    }
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
    let resolution = resolve_module(cfg, dir, spec)?;
    ctx.scope(|mut scope| {
        let cache = scope.value(Value::object(cache));
        let exports = load_resolved_scoped(&mut scope, cfg, cache, &resolution, None)?;
        Ok(scope.finish(exports))
    })
}

/// Compile and execute one CommonJS file, returning its `module.exports`.
pub(crate) fn cjs_instantiate_file(
    ctx: &mut NativeCtx<'_>,
    cfg: &Arc<CjsConfig>,
    abs: &Path,
    source: &str,
) -> Result<Value, NativeError> {
    let resolution = CjsResolution::file(abs.to_path_buf());
    ctx.scope(|mut scope| {
        let cache = scope.bare_object()?;
        let exports = load_resolved_scoped(&mut scope, cfg, cache, &resolution, Some(source))?;
        Ok(scope.finish(exports))
    })
}
