//! CommonJS module system for the active runtime.
//!
//! Implements Node's `require` / `module.exports` / `__dirname` semantics on top
//! of the VM. Each CommonJS file runs inside a wrapper function
//! `(function (exports, require, module, __filename, __dirname) { ... })` built
//! by [`otter_vm::Interpreter::create_commonjs_wrapper`], and `require` is a
//! per-module native function that re-enters the runtime synchronously to
//! load dependencies.
//!
//! # Contents
//! - [`CjsConfig`] - capability snapshot, hosted-module list and resolver
//!   shared by every `require` of a runtime.
//! - [`cjs_instantiate_file`] - compile + execute one CommonJS entry file.
//! - `resolve_module` - hosted lookup, then Node's CommonJS resolution through
//!   the runtime's module loader (`exports` / `imports`, package graph), and
//!   cache keys.
//! - `cjs_load` - load hosted modules, files, and native addons through the
//!   shared module-record cache.
//! - [`create_require`] - `module.createRequire`: the `require` of a user
//!   module at any path.
//! - [`user_may_load`] - which builtin ids code outside the builtins reaches.
//! - `module_not_found` / `unknown_builtin` - Node's errors for a request that
//!   names nothing, with the require stack that led to it.
//! - [`SCHEME_ONLY_BUILTINS`] - builtins a user module reaches only as
//!   `node:<name>`.
//!
//! # Invariants
//! - A module's `require`, `require.resolve` and `require.resolve.paths` are
//!   plain native functions: the referrer they resolve from rides their
//!   traced captures and the config is the runtime's [`CommonJsHost`], so a
//!   startup image carries the builtins a build loaded.
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
//! - Builtin discovery returns `None` before cache preparation or installation.
//!   A discovered builtin retains every installer/allocation error unchanged;
//!   only a caller whose contract permits absence may consume `None`.
//! - Hosted namespace and CommonJS-value installers run directly in the
//!   loader's existing handle scope. Namespace cache publication and
//!   `require.cache` publication happen before that scope closes.
//! - Filesystem capabilities are checked before any module file is read.
//!   Package manifests are resolution metadata the shared resolver reads, as
//!   for `import`. Native addons additionally pass through the configured
//!   loader's FFI capability check.
//! - A bare specifier names a builtin only when the requester is allowed to
//!   spell it that way: builtins require each other bare, while a user module
//!   naming a [`SCHEME_ONLY_BUILTINS`] entry gets `node_modules` resolution.
//! - An internal id (`internal/*`, and the engine's `__*` natives) names a
//!   builtin only for a builtin's own `require` and for the runtime's own
//!   loads ([`cjs_load_builtin`]). User code's `require`, `createRequire`,
//!   `require.resolve` and `import` resolve it as a package and fail the way
//!   Node's do; under `--expose-internals` the `internal/*` ids except the
//!   realm module open to them, as in Node.
//! - A user `require` of a `node:` id that names no builtin it may load throws
//!   `ERR_UNKNOWN_BUILTIN_MODULE`; `require.resolve` of one reports
//!   `MODULE_NOT_FOUND`, and answers a builtin with the request as written.
//! - `MODULE_NOT_FOUND` names the files whose first `require`s led to the
//!   failing request, newest first, in its message and as `requireStack`.
//!   Builtins are never part of that stack.
//! - Re-entry uses [`otter_vm::Interpreter::run_callable_sync`] and the
//!   code-space-linked wrapper from `create_commonjs_wrapper`; the unsafe
//!   `Interpreter::run` (which swaps `code_space`) is never called nested.
//!
//! # See also
//! - [`crate::CommonJsAddonLoader`]
//! - [`crate::RuntimeBuilder::commonjs_addon_loader`]

use otter_vm::ValueWindow;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use otter_vm::{Attr, Local, NativeCtx, NativeScope, Value, object};

use crate::{
    CapabilitySet, CommonJsAddonLoader, HostedModule, RuntimeNativeError as NativeError,
    RuntimeTaskSpawner, runtime_type_error,
};

/// The CommonJS configuration a runtime's `require` functions share:
/// capability snapshot, hosted modules, task spawner, optional native-addon
/// loader, and the resolver.
#[derive(Debug)]
pub(crate) struct CjsConfig {
    pub(crate) capabilities: CapabilitySet,
    pub(crate) hosted: Vec<HostedModule>,
    pub(crate) runtime_task_spawner: Option<RuntimeTaskSpawner>,
    pub(crate) addon_loader: Option<CommonJsAddonLoader>,
    /// Node's CommonJS resolution over the runtime's loader configuration:
    /// the same package graph, `exports` / `imports` maps and extensions as
    /// `import`, under the `require` conditions.
    pub(crate) resolver: crate::module_loader::ModuleLoader,
    /// Whether this run reports each file it requires to a watching
    /// parent. Set once per run, from the environment the parent spawned
    /// it with; see [`watch_reporting_requested`].
    pub(crate) report_watch_dependencies: bool,
    /// `--expose-internals`: user code may load the `internal/*` builtins
    /// (all but the realm module), as Node lets it under that switch.
    pub(crate) expose_internals: bool,
}

/// The run's CommonJS configuration as a host service, so a static native
/// such as [`create_require`] can build a `require` without capturing it.
#[derive(Debug, Clone)]
pub(crate) struct CommonJsHost(pub(crate) Arc<CjsConfig>);

/// Whether the parent that spawned this process asked to be told what it
/// requires.
///
/// Watch mode learns a test file's dependencies from the child that loads
/// them, and asks for that by setting `WATCH_REPORT_DEPENDENCIES` in the
/// child's environment.
#[must_use]
pub fn watch_reporting_requested() -> bool {
    std::env::var_os("WATCH_REPORT_DEPENDENCIES").is_some_and(|value| !value.is_empty())
}

/// Post one imported module URL to the watching parent.
///
/// The ESM side reports what it linked, not what it resolved: the graph is
/// built before any of it evaluates, so the whole batch is known at once.
pub(crate) fn report_watch_imports<'a>(
    ctx: &mut NativeCtx<'_>,
    urls: impl Iterator<Item = &'a str> + Clone,
) {
    let _ = ctx.scope(|mut scope| -> Result<Value, NativeError> {
        let nothing = scope.undefined();
        let Some(process) = scope.global("process") else {
            return Ok(scope.finish(nothing));
        };
        let send = scope.get(process, "send")?;
        if !scope.is_callable(send) {
            return Ok(scope.finish(nothing));
        }
        let count = urls.clone().count();
        if count == 0 {
            return Ok(scope.finish(nothing));
        }
        let message = scope.object()?;
        let files = scope.array(count)?;
        for (index, url) in urls.enumerate() {
            let value = scope.string(url)?;
            scope.set_index(files, index, value)?;
        }
        scope.set(message, "watch:import", files)?;
        scope.call(send, process, &[message])?;
        Ok(scope.finish(nothing))
    });
}

/// Post one required file to the watching parent over the IPC channel.
///
/// Node does this from its loaders; loading here is native, so the report
/// is made here instead. A run with no channel — or none of this asked for
/// — has nothing to post, and a failed post is not the requiring module's
/// problem, so nothing propagates.
fn report_watch_dependency(ctx: &mut NativeCtx<'_>, filename: &str) {
    let _ = ctx.scope(|mut scope| -> Result<Value, NativeError> {
        let Some(process) = scope.global("process") else {
            let nothing = scope.undefined();
            return Ok(scope.finish(nothing));
        };
        let send = scope.get(process, "send")?;
        if !scope.is_callable(send) {
            let nothing = scope.undefined();
            return Ok(scope.finish(nothing));
        }
        let message = scope.object()?;
        let files = scope.array(1)?;
        let path = scope.string(filename)?;
        scope.set_index(files, 0, path)?;
        scope.set(message, "watch:require", files)?;
        scope.call(send, process, &[message])?;
        let nothing = scope.undefined();
        Ok(scope.finish(nothing))
    });
}

/// Run a builtin CommonJS module the product build compiled and return its
/// `module.exports`. For builtin modules whose natural implementation is a
/// self-contained JS class or helper set (e.g. `events`, `node:test`).
///
/// The builtin's wrapper text is exactly a file module's
/// (`(function (exports, require, module, __filename, __dirname) { ... })`),
/// and its embedded module links after host verification, on its first
/// `require` only. The supplied `module` is the live record already present
/// in the shared cache and `require` is the importing module's canonical
/// resolver. A builtin's dependency loads and `module.exports` replacements
/// therefore participate in exactly the same singleton and circular-loading
/// semantics as file modules. `__filename`/`__dirname` use the builtin's URL
/// for diagnostics.
///
/// # Errors
/// Returns a native error on allocation, verification, or runtime failure.
pub fn run_builtin_cjs_shim<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    unit: &'static otter_vm::EmbeddedCommonJs,
    module: Local<'scope>,
    require: Local<'scope>,
) -> Result<Local<'scope>, NativeError> {
    let exports = scope.get(module, "exports")?;
    let module_name = scope.string(unit.url)?;
    let wrapper = scope.embedded_commonjs_wrapper(unit)?;
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

/// Builtins reachable only through the `node:` scheme. A user module
/// naming one of these bare gets ordinary `node_modules` resolution, so a
/// package called `test` shadows nothing. Builtins requiring each other
/// still use the bare spelling, which is why the caller says whether the
/// request came from one of them.
pub const SCHEME_ONLY_BUILTINS: &[&str] = &[
    "dtls",
    "ffi",
    "sea",
    "sqlite",
    "quic",
    "test",
    "test/reporters",
    "vfs",
];

/// The builtin that supplies `primordials` and `internalBinding` to the
/// others. Like Node's loader, it stays out of user code's reach even under
/// `--expose-internals`.
const REALM_MODULE: &str = "internal/bootstrap/realm";

/// Whether code outside the builtins may load the builtin `specifier`, bare
/// or `node:`-prefixed.
///
/// The internal ids — Node's `internal/*` and the engine's `__*` natives —
/// are the builtins' own dependencies. `expose_internals`
/// (`--expose-internals`) opens the `internal/*` ones except the realm
/// module, exactly the set Node opens.
#[must_use]
pub(crate) fn user_may_load(specifier: &str, expose_internals: bool) -> bool {
    let id = specifier.strip_prefix("node:").unwrap_or(specifier);
    if id.starts_with("__") {
        return false;
    }
    if id.starts_with("internal/") {
        return expose_internals && id != REALM_MODULE;
    }
    true
}

/// The hosted row registered under exactly `name`.
fn hosted_row(cfg: &CjsConfig, name: &str) -> Option<HostedModule> {
    cfg.hosted
        .iter()
        .copied()
        .find(|hosted| hosted.specifier() == name)
}

/// Resolve a builtin (hosted) module by specifier for one requester.
///
/// `<id>` and `node:<id>` reach the same row — the `node:<id>` one when it is
/// registered, `<id>` otherwise — so every spelling shares one module record.
/// A requester outside the builtins reaches only what [`user_may_load`]
/// admits, and a [`SCHEME_ONLY_BUILTINS`] entry only through the scheme.
fn resolve_builtin(cfg: &CjsConfig, spec: &str, from_builtin: bool) -> Option<HostedModule> {
    let (id, scheme) = match spec.strip_prefix("node:") {
        Some(id) => (id, true),
        None => (spec, false),
    };
    if !from_builtin
        && (!user_may_load(id, cfg.expose_internals)
            || (!scheme && SCHEME_ONLY_BUILTINS.contains(&id)))
    {
        return None;
    }
    let prefixed;
    let canonical = if scheme {
        spec
    } else {
        prefixed = format!("node:{id}");
        &prefixed
    };
    hosted_row(cfg, canonical).or_else(|| hosted_row(cfg, id))
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
    /// The require chain of the module that asked for this one.
    parent: Option<Arc<RequireChain>>,
}

impl CjsResolution {
    fn file(path: PathBuf, parent: Option<Arc<RequireChain>>) -> Self {
        let filename = path.to_string_lossy().into_owned();
        let dir = path
            .parent()
            .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
        Self {
            key: filename.clone(),
            filename,
            dir,
            target: CjsTarget::File(path),
            parent,
        }
    }

    /// The referrer this module's own `require` resolves from.
    ///
    /// A file heads its own require chain; a builtin adds nothing to the
    /// chain it was loaded through, as Node's builtins appear in no require
    /// stack.
    fn referrer(&self) -> CjsReferrer {
        let (file, chain) = match &self.target {
            CjsTarget::File(path) => (
                Some(path.clone()),
                Some(Arc::new(RequireChain {
                    filename: self.filename.clone(),
                    parent: self.parent.clone(),
                })),
            ),
            CjsTarget::Hosted(_) => (None, self.parent.clone()),
        };
        CjsReferrer {
            dir: self.dir.clone(),
            file,
            chain,
        }
    }
}

/// Why `resolve_module` found nothing.
#[derive(Debug, Clone, Copy)]
enum Unresolved {
    /// No builtin, package or file answers to the specifier.
    NotFound,
    /// A `node:` specifier naming no builtin the requester may load.
    UnknownBuiltin,
}

/// Resolve one specifier and derive its sole cache key.
///
/// The selected hosted row is authoritative for aliases, so both
/// `fs/promises` and `node:fs/promises` use `node:fs/promises` when that is the
/// registered specifier. Files use their canonical absolute path. A `node:`
/// specifier names a builtin or nothing: no file answers to it.
fn resolve_module(
    cfg: &CjsConfig,
    referrer: &CjsReferrer,
    spec: &str,
    from_builtin: bool,
) -> Result<CjsResolution, Unresolved> {
    if let Some(hosted) = resolve_builtin(cfg, spec, from_builtin) {
        let key = hosted.specifier().to_string();
        return Ok(CjsResolution {
            filename: key.clone(),
            key,
            dir: referrer.dir.clone(),
            target: CjsTarget::Hosted(hosted),
            parent: referrer.chain.clone(),
        });
    }
    if spec.starts_with("node:") {
        return Err(Unresolved::UnknownBuiltin);
    }
    let path = cfg
        .resolver
        .resolve_require(spec, referrer.file.as_deref(), &referrer.dir)
        .map_err(|_| Unresolved::NotFound)?;
    Ok(CjsResolution::file(path, referrer.chain.clone()))
}

/// One file in the chain of first `require`s that led to a module, newest
/// first: what Node prints as the `Require stack` of a failed request.
#[derive(Debug)]
struct RequireChain {
    filename: String,
    parent: Option<Arc<RequireChain>>,
}

/// The module a `require` resolves from: its directory, its file when it has
/// one (a hosted builtin has none), and the require chain that loaded it.
#[derive(Debug, Clone)]
struct CjsReferrer {
    dir: PathBuf,
    file: Option<PathBuf>,
    chain: Option<Arc<RequireChain>>,
}

/// Node's `ERR_UNKNOWN_BUILTIN_MODULE`, for a `node:` request that names no
/// builtin the requester may load.
fn unknown_builtin(spec: &str) -> NativeError {
    NativeError::Coded {
        kind: otter_vm::error_classes::ErrorKind::Error,
        code: "ERR_UNKNOWN_BUILTIN_MODULE",
        message: format!("No such built-in module: {spec}"),
    }
}

/// Node's `MODULE_NOT_FOUND`: `Cannot find module '<request>'`, then the
/// require stack of the module that asked — its own file and the files whose
/// first `require`s led to it — which the error also carries as
/// `requireStack`.
fn module_not_found(
    ctx: &mut NativeCtx<'_>,
    request: &str,
    chain: Option<&Arc<RequireChain>>,
) -> NativeError {
    let stack: Vec<&str> = std::iter::successors(chain, |link| link.parent.as_ref())
        .map(|link| link.filename.as_str())
        .collect();
    let mut message = format!("Cannot find module '{request}'");
    if !stack.is_empty() {
        message.push_str("\nRequire stack:\n- ");
        message.push_str(&stack.join("\n- "));
    }
    let built = ctx.scope(|mut scope| -> Result<Value, NativeError> {
        let constructor = scope.error_constructor(otter_vm::error_classes::ErrorKind::Error);
        let text = scope.string(&message)?;
        let error = scope.construct(constructor, &[text])?;
        let flags = Attr::data().to_flags();
        let code = scope.string("MODULE_NOT_FOUND")?;
        scope.define(error, "code", code, flags)?;
        let frames = scope.array(stack.len())?;
        for (index, filename) in stack.iter().enumerate() {
            let filename = scope.string(filename)?;
            scope.set_index(frames, index, filename)?;
        }
        scope.define(error, "requireStack", frames, flags)?;
        Ok(scope.finish(error))
    });
    match built {
        Ok(error) => ctx.throw_value("require", error),
        Err(error) => error,
    }
}

/// Build a per-module `require` native function bound to its referrer.
///
/// The shared cache and the referrer ride the function's traced captures
/// ([`referrer_captures`]) and the config is the runtime's [`CommonJsHost`],
/// so the callable holds no Rust state and a startup image can carry it.
fn make_require<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    cache: Local<'_>,
    referrer: &CjsReferrer,
    from_builtin: bool,
) -> Result<Local<'scope>, NativeError> {
    let [dir, file, chain, builtin] = referrer_captures(scope, referrer, from_builtin)?;
    let require = scope.native_with_captures(
        "require",
        1,
        &[cache, dir, file, chain, builtin],
        require_call,
    )?;
    scope.define(require, "cache", cache, Attr::data().to_flags())?;
    let resolve = scope.native_with_captures(
        "resolve",
        1,
        &[dir, file, chain, builtin],
        require_resolve_call,
    )?;
    let paths =
        scope.native_with_captures("paths", 1, &[dir, file, chain, builtin], require_paths_call)?;
    scope.define(resolve, "paths", paths, Attr::data().to_flags())?;
    scope.define(require, "resolve", resolve, Attr::data().to_flags())?;
    Ok(require)
}

/// The captures that name a referrer: its directory, its file or
/// `undefined`, its require chain innermost first and joined by NUL (no path
/// holds one), and whether it is a builtin.
fn referrer_captures<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    referrer: &CjsReferrer,
    from_builtin: bool,
) -> Result<[Local<'scope>; 4], NativeError> {
    let dir = scope.string(&referrer.dir.to_string_lossy())?;
    let file = match &referrer.file {
        Some(file) => scope.string(&file.to_string_lossy())?,
        None => scope.undefined(),
    };
    let chain: Vec<&str> =
        std::iter::successors(referrer.chain.as_ref(), |link| link.parent.as_ref())
            .map(|link| link.filename.as_str())
            .collect();
    let chain = scope.string(&chain.join("\0"))?;
    let builtin = scope.boolean(from_builtin);
    Ok([dir, file, chain, builtin])
}

/// The referrer and builtin flag [`referrer_captures`] recorded, read from
/// `captures` starting at `first`.
fn read_referrer(
    ctx: &NativeCtx<'_>,
    captures: ValueWindow<'_>,
    first: usize,
) -> (CjsReferrer, bool) {
    let heap = ctx.heap();
    let dir = PathBuf::from(crate::runtime_arg_to_string(captures, first, heap));
    let file = captures
        .get(first + 1)
        .filter(|value| !value.is_undefined())
        .map(|_| PathBuf::from(crate::runtime_arg_to_string(captures, first + 1, heap)));
    let chain = crate::runtime_arg_to_string(captures, first + 2, heap);
    let chain = chain
        .split('\0')
        .filter(|filename| !filename.is_empty())
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .fold(None, |parent, filename| {
            Some(Arc::new(RequireChain {
                filename: filename.to_owned(),
                parent,
            }))
        });
    let from_builtin = captures
        .get(first + 3)
        .is_some_and(|value| value.as_boolean() == Some(true));
    (CjsReferrer { dir, file, chain }, from_builtin)
}

/// The runtime's CommonJS config, which every `require` shares.
fn commonjs_config(ctx: &NativeCtx<'_>, name: &'static str) -> Result<Arc<CjsConfig>, NativeError> {
    ctx.host_service::<CommonJsHost>()
        .map(|CommonJsHost(cfg)| cfg)
        .ok_or_else(|| runtime_type_error(name, "CommonJS is not enabled"))
}

fn require_call(
    ctx: &mut NativeCtx<'_>,
    args: ValueWindow<'_>,
    captures: ValueWindow<'_>,
) -> Result<Value, NativeError> {
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
    let cfg = commonjs_config(ctx, "require")?;
    let (referrer, from_builtin) = read_referrer(ctx, captures, 1);
    cjs_load(ctx, &cfg, cache, &referrer, &spec, from_builtin)
}

/// `require.resolve` for one referrer.
///
/// It answers with the same resolution `require` itself would take — a
/// builtin with the request as written, as Node does, a file with its
/// canonical path — and reports anything unresolvable, a `node:` id naming
/// no builtin included, with `MODULE_NOT_FOUND`.
fn require_resolve_call(
    ctx: &mut NativeCtx<'_>,
    args: ValueWindow<'_>,
    captures: ValueWindow<'_>,
) -> Result<Value, NativeError> {
    let spec = crate::runtime_arg_to_string(args, 0, ctx.heap());
    if spec.is_empty() {
        return Err(runtime_type_error(
            "require.resolve",
            "module specifier is required",
        ));
    }
    let cfg = commonjs_config(ctx, "require.resolve")?;
    let (referrer, from_builtin) = read_referrer(ctx, captures, 0);
    let Ok(resolution) = resolve_module(&cfg, &referrer, &spec, from_builtin) else {
        return Err(module_not_found(ctx, &spec, referrer.chain.as_ref()));
    };
    let answer = match resolution.target {
        CjsTarget::Hosted(_) => spec.as_str(),
        CjsTarget::File(_) => resolution.filename.as_str(),
    };
    ctx.scope(|mut scope| {
        let filename = scope.string(answer)?;
        Ok(scope.finish(filename))
    })
}

/// `require.resolve.paths`: the directories a request is looked up in, as
/// Node lists them. A builtin request answers `null`, a relative one its
/// referrer's directory (`.` for a referrer without a file), and any other
/// the `node_modules` chain above the referrer.
fn require_paths_call(
    ctx: &mut NativeCtx<'_>,
    args: ValueWindow<'_>,
    captures: ValueWindow<'_>,
) -> Result<Value, NativeError> {
    let spec = crate::runtime_arg_to_string(args, 0, ctx.heap());
    let cfg = commonjs_config(ctx, "require.resolve.paths")?;
    let (referrer, from_builtin) = read_referrer(ctx, captures, 0);
    let entries: Option<Vec<String>> = if resolve_builtin(&cfg, &spec, from_builtin).is_some() {
        None
    } else if is_relative_request(&spec) {
        Some(vec![match &referrer.file {
            Some(_) => referrer.dir.to_string_lossy().into_owned(),
            None => ".".to_owned(),
        }])
    } else {
        let chain: Vec<String> = referrer
            .dir
            .ancestors()
            .filter(|ancestor| !ancestor.as_os_str().is_empty())
            .map(|ancestor| ancestor.join("node_modules").to_string_lossy().into_owned())
            .collect();
        (!chain.is_empty()).then_some(chain)
    };
    ctx.scope(|mut scope| {
        let Some(entries) = entries else {
            let null = scope.null();
            return Ok(scope.finish(null));
        };
        let array = scope.array(entries.len())?;
        for (index, entry) in entries.iter().enumerate() {
            let value = scope.string(entry)?;
            scope.set_index(array, index, value)?;
        }
        Ok(scope.finish(array))
    })
}

/// Node's relative request: `.` or `..` alone or before a separator.
fn is_relative_request(spec: &str) -> bool {
    let rest = spec.strip_prefix("..").or_else(|| spec.strip_prefix('.'));
    rest.is_some_and(|rest| {
        rest.is_empty() || rest.starts_with('/') || (cfg!(windows) && rest.starts_with('\\'))
    })
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

/// Publish a host-produced `exports` value on the module and mark it loaded.
fn finish_module<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    module: Local<'_>,
    exports: Local<'_>,
) -> Result<Local<'scope>, NativeError> {
    scope.set(module, "exports", exports)?;
    finish_evaluated_module(scope, module)
}

/// Mark a module whose own code ran as loaded and answer its
/// `module.exports`. The code owns that property — it may have replaced it
/// with an accessor — so the loader only reads it, as Node's does.
fn finish_evaluated_module<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    module: Local<'_>,
) -> Result<Local<'scope>, NativeError> {
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
        let require = make_require(
            scope,
            cache,
            &resolution.referrer(),
            matches!(resolution.target, CjsTarget::Hosted(_)),
        )?;
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
                // `read` gates filesystem access, so it is checked at each
                // point that actually opens the file. The entry arrives with
                // its source already in hand: that is code loading, not
                // `fs_read`, and it is reached identically by the ESM path,
                // which requires no capability either.
                let denied = || {
                    runtime_type_error(
                        "require",
                        format!("permission denied for '{}'", resolution.filename),
                    )
                };
                if path.extension().is_some_and(|ext| ext == "node") {
                    if !cfg.capabilities.read.matches_path(path) {
                        return Err(denied());
                    }
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
                // A data file is not code: `require` parses it and publishes
                // the value, the way an `import` of the same file does.
                if entry_source.is_none()
                    && let Some(format) = path
                        .extension()
                        .and_then(|extension| extension.to_str())
                        .and_then(crate::data_modules::DataFormat::from_extension)
                {
                    if !cfg.capabilities.read.matches_path(path) {
                        return Err(denied());
                    }
                    let bytes = std::fs::read(path).map_err(|err| {
                        runtime_type_error(
                            "require",
                            format!("io error for '{}': {err}", resolution.filename),
                        )
                    })?;
                    let source = crate::data_modules::data_module_commonjs_source(format, &bytes)
                        .map_err(|err| {
                        runtime_type_error(
                            "require",
                            format!("{}: {}", resolution.filename, err.message),
                        )
                    })?;
                    let dirname = scope.string(&resolution.dir.to_string_lossy())?;
                    let wrapper = scope.commonjs_wrapper(&resolution.filename, &source)?;
                    scope.call(
                        wrapper,
                        record.exports,
                        &[record.exports, require, record.module, record.id, dirname],
                    )?;
                    return finish_evaluated_module(scope, record.module);
                }
                let owned_source;
                let source = if let Some(source) = entry_source {
                    source
                } else {
                    if !cfg.capabilities.read.matches_path(path) {
                        return Err(denied());
                    }
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
                finish_evaluated_module(scope, record.module)
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

/// Resolve and load a module by specifier from `referrer`. Returns the
/// module's exports value.
fn cjs_load(
    ctx: &mut NativeCtx<'_>,
    cfg: &Arc<CjsConfig>,
    cache: object::JsObject,
    referrer: &CjsReferrer,
    spec: &str,
    from_builtin: bool,
) -> Result<Value, NativeError> {
    let resolution = match resolve_module(cfg, referrer, spec, from_builtin) {
        Ok(resolution) => resolution,
        Err(Unresolved::UnknownBuiltin) => return Err(unknown_builtin(spec)),
        Err(Unresolved::NotFound) => {
            return Err(module_not_found(ctx, spec, referrer.chain.as_ref()));
        }
    };
    if cfg.report_watch_dependencies
        && let CjsTarget::File(path) = &resolution.target
    {
        let path = path.to_string_lossy().into_owned();
        report_watch_dependency(ctx, &path);
    }
    ctx.scope(|mut scope| {
        let cache = scope.value(Value::object(cache));
        let exports = load_resolved_scoped(&mut scope, cfg, cache, &resolution, None)?;
        Ok(scope.finish(exports))
    })
}

/// Load a builtin the runtime itself asks for, by the name it is registered
/// under, through the CommonJS pipeline, so its own `require` graph, aliases,
/// and cycles behave exactly as they do for a `require()` from JavaScript.
///
/// The runtime's own requests reach the internal builtins — the standard
/// streams, the IPC channel, a builtin started as the main program, and
/// `process.getBuiltinModule`, through which the realm installers reach
/// theirs. A bare [`SCHEME_ONLY_BUILTINS`] name is a builtin only inside the
/// builtin graph.
///
/// Returns `None` when discovery finds no hosted builtin, before cache
/// preparation or installation.
///
/// # Errors
/// Returns the original native error from cache preparation or installation.
pub(crate) fn cjs_load_builtin<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    cfg: &Arc<CjsConfig>,
    specifier: &str,
) -> Result<Option<Local<'scope>>, NativeError> {
    let hosted = if specifier.starts_with("node:") {
        hosted_row(cfg, specifier)
    } else if SCHEME_ONLY_BUILTINS.contains(&specifier) {
        None
    } else {
        hosted_row(cfg, &format!("node:{specifier}")).or_else(|| hosted_row(cfg, specifier))
    };
    let Some(hosted) = hosted else {
        return Ok(None);
    };
    load_builtin(scope, cfg, hosted).map(Some)
}

/// Load a builtin user code asked for by name, as `require` from a user
/// module would find it ([`user_may_load`]).
///
/// The ESM side calls this for a builtin that publishes only a CommonJS value:
/// the namespace it synthesizes has to hold the same object `require` would
/// return, and that object can only be produced by running the installer with
/// a working `require` in hand.
///
/// Returns `None` when the name is no builtin user code may load, before
/// cache preparation or installation.
///
/// # Errors
/// Returns the original native error from cache preparation or installation.
pub(crate) fn cjs_load_user_builtin<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    cfg: &Arc<CjsConfig>,
    specifier: &str,
) -> Result<Option<Local<'scope>>, NativeError> {
    let Some(hosted) = resolve_builtin(cfg, specifier, false) else {
        return Ok(None);
    };
    load_builtin(scope, cfg, hosted).map(Some)
}

fn load_builtin<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    cfg: &Arc<CjsConfig>,
    hosted: HostedModule,
) -> Result<Local<'scope>, NativeError> {
    let key = hosted.specifier().to_string();
    let resolution = CjsResolution {
        filename: key.clone(),
        key,
        dir: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        target: CjsTarget::Hosted(hosted),
        parent: None,
    };
    let cache = canonical_cache(scope)?;
    load_resolved_scoped(scope, cfg, cache, &resolution, None)
}

/// `module.createRequire(filename)`: the `require` a user module at
/// `filename` has — Node's resolution from that file's directory, the realm's
/// one module cache, and `filename` at the head of the require stack of
/// anything it cannot find.
///
/// `filename` is an absolute path; `node:module` checks the argument and turns
/// a URL into one before it calls this.
///
/// # Errors
/// Returns a native error when the run has no CommonJS configuration or an
/// allocation fails.
pub fn create_require(
    ctx: &mut NativeCtx<'_>,
    args: ValueWindow<'_>,
) -> Result<Value, NativeError> {
    commonjs_config(ctx, "createRequire")?;
    let filename = crate::runtime_arg_to_string(args, 0, ctx.heap());
    let path = PathBuf::from(&filename);
    let referrer = CjsReferrer {
        dir: path
            .parent()
            .map_or_else(|| PathBuf::from("."), Path::to_path_buf),
        file: Some(path),
        chain: Some(Arc::new(RequireChain {
            filename,
            parent: None,
        })),
    };
    ctx.scope(|mut scope| {
        let cache = canonical_cache(&mut scope)?;
        let require = make_require(&mut scope, cache, &referrer, false)?;
        Ok(scope.finish(require))
    })
}

/// The realm's one require cache. Every load path — the CommonJS entry
/// file, `require()` chains, and ESM imports of hosted builtins — must
/// share it, or a builtin like `net` is instantiated twice and the second
/// copy's module state (handle tables, dispatch hooks) orphans the first.
fn canonical_cache<'scope>(
    scope: &mut NativeScope<'scope, '_>,
) -> Result<Local<'scope>, NativeError> {
    const SLOT: &str = "__otterRequireCache";
    if let Some(existing) = scope.global(SLOT) {
        return Ok(existing);
    }
    let cache = scope.bare_object()?;
    let globals = scope.global_this();
    // Non-enumerable: the Node test harness flags unknown enumerable
    // globals as leaks.
    scope.define(
        globals,
        SLOT,
        cache,
        otter_vm::Attr {
            writable: true,
            enumerable: false,
            configurable: true,
        }
        .to_flags(),
    )?;
    Ok(cache)
}

/// Compile and execute one CommonJS file, returning its `module.exports`.
pub(crate) fn cjs_instantiate_file(
    ctx: &mut NativeCtx<'_>,
    cfg: &Arc<CjsConfig>,
    abs: &Path,
    source: &str,
) -> Result<Value, NativeError> {
    let resolution = CjsResolution::file(abs.to_path_buf(), None);
    // The entry counts too: a watching parent maps a file to the tests that
    // depend on it, and a test file is its own first dependency.
    if cfg.report_watch_dependencies {
        let path = resolution.filename.clone();
        report_watch_dependency(ctx, &path);
    }
    ctx.scope(|mut scope| {
        let cache = canonical_cache(&mut scope)?;
        let exports = load_resolved_scoped(&mut scope, cfg, cache, &resolution, Some(source))?;
        Ok(scope.finish(exports))
    })
}
