# CommonJS Module System — Design (Phase 0, Node conformance)

## Problem
`require` / `module.exports` / `__dirname` do not exist in the active runtime.
"CommonJS" support today = parse-mode only (`run_file` CJS branch at
`crates/otter-runtime/src/lib.rs:2944` calls `run_script_with_context`, a plain
script). Node `test/parallel` is overwhelmingly CommonJS (`require('../common')`),
so ~0 tests run. This is the foundational gate before any Node module conformance.

## Approach
Node's real module wrapper, reentry-safe:
- Each CJS file compiled as `(function (exports, require, module, __filename, __dirname) { <src> })`.
- Invoke with the 5 bindings; `module.exports` holds the result.
- `require` is a per-module native closure (captures the shared cache + the
  module's directory) that re-enters the runtime synchronously to load deps.

## Key VM facts (verified)
- `Interpreter::build_function_constructor_from_parts(parts)` (eval_ops.rs:469,
  `pub(crate)`) builds a `Function(args, body)` callable Value. Reentry-safe: it
  links into the interpreter code space (NOT `Interpreter::run`, which swaps
  `code_space` and is unsafe nested). Exposed via new pub `create_commonjs_wrapper`.
- `Interpreter::run_callable_sync(context, callee, this, args)` (call_ops.rs:1521,
  pub) — reentry-safe invocation. Use to call the wrapper + user `toString` etc.
- `NativeCtx::new_with_call_info_and_context(interp, NativeCallInfo::default_call(), Some(ctx))`
  (pub) — build a ctx at entry from `&mut Interpreter` + an `ExecutionContext`.
- `NativeCtx::interp_mut_and_context()` (pub) — `(interp, Option<ctx>)` for nested re-entry.
- `ctx.alloc_object()`, `ctx.heap()/heap_mut()`, `runtime_string_value(ctx,&str)`.
- `object::get(obj, heap, key)` / `object::set(obj, heap, key, val)`.
- `native_value_with_captures(heap, name, captures: SmallVec<[Value;4]>, closure)`
  (native_function.rs:1037) — build the per-module `require` closure.
- `otter_compiler::compile_script_source(src, kind, spec)` (pub) → bytecode;
  `Interpreter::link_module(bytecode)` (pub) → `ExecutionContext`.
- Entry `ExecutionContext`: link an empty compiled script once per run.
- `RuntimeConfig.capabilities: CapabilitySet`, `RuntimeConfig.hosted_modules: Vec<HostedModule>` (same crate).
- Hosted builtin namespace built by `HostedModuleInstall::install(interp, caps)` → `JsObject`.

## Module: `crates/otter-runtime/src/commonjs.rs`
```
struct CjsConfig { capabilities: CapabilitySet, hosted: Vec<HostedModule> }

// cache: a plain JS object used as require.cache (key = resolved id -> module.exports).
//        keeps exports rooted by GC. circular: insert BEFORE running wrapper.

fn cjs_load(ctx: &mut NativeCtx, cfg: &Arc<CjsConfig>, cache: JsObject, dir: &Path, spec: &str)
    -> Result<Value, NativeError>
  1. builtin?  spec == "node:X" or bare matches a hosted specifier:
       cache hit -> return; else build hosted namespace, cache, return.
  2. file: resolve dir.join(spec) probing [as-is, .js, .cjs, .json, /index.js, /index.cjs];
       canonicalize -> id. cache hit -> return. capability read-check.
       read source. -> cjs_instantiate_file.
  (bare non-builtin / node_modules resolution: error for now — later.)

fn cjs_instantiate_file(ctx, cfg, cache, abs: &Path, source: &str) -> Result<Value, NativeError>
  - exports = alloc_object(); module = alloc_object{ exports, id, filename, loaded:false }
  - cache.set(id, exports)                      // circular guard BEFORE run
  - wrapper = interp.create_commonjs_wrapper(source)
  - child_require = native_value_with_captures([cache, dir_string], |ctx,args,caps| cjs_load(...))
  - __filename=string(abs), __dirname=string(parent)
  - run_callable_sync(context, wrapper, this=exports, [exports, child_require, module, __filename, __dirname])
  - reread module.exports (may be reassigned); cache.set(id, that); return.
```
`.json`: load as `module.exports = JSON.parse(text)` (later slice).

## Wiring (`lib.rs`)
- Add `RuntimeConfig.commonjs_enabled: bool`.
- Builder: `RuntimeBuilder::with_nodejs_modules(self)` + `OtterBuilder::with_nodejs_modules` → set flag.
- `run_file_with_context` / `check`: when `commonjs_enabled` and source is CJS
  (package_type==CommonJs, `.cjs`, or ambiguous non-module `.js`) → `run_commonjs_file`.
- `Runtime::run_commonjs_file(path)`: canonicalize, read, build CjsConfig from
  `self.config`, link empty entry context, build NativeCtx, alloc cache,
  `cjs_instantiate_file`, drain microtasks, return ExecutionResult (+process exit code).

## Probe gate
```
// dep.cjs:  module.exports = { hi: 42 };
// main.cjs: const d=require('./dep.cjs'); const fs=require('node:fs');
//           console.log(d.hi, typeof fs.readFileSync, __dirname);
```
`otter --allow-all main.cjs` (with commonjs enabled) must print `42 function <dir>`.
Proves builtin require + relative require + wrapper + cache + circular guard.

## After gate
Phase 0.5 harness revival, Phase 1 `common` shim, Phase 2 assert/path/util
(register as hosted modules so both ESM `node:assert` and CJS `require('assert')` work).

---

## Phase 0.5 — DONE
- `otter-node-compat` moved `crates-legacy/` → `crates/otter-node-compat`, in workspace,
  builds. Spawn fixed: `cargo build -p otter-cli` (was `otterjs`); dropped the binary
  `--timeout` flag (active CLI has none; external watchdog enforces limits).
- `scripts/fetch-node-tests.sh v24.x` fetched **4067** parallel test files.
- `NODE_CONFORMANCE.md` is now **auto-generated from each run** (`write_conformance_markdown`
  in `lib.rs`) — summary + per-module pass-rate table. Never hand-edited.
- Baseline confirmed: `node-compat path --limit 10` runs, all fail with
  `Cannot find module 'assert'` (expected — modules not ported, `../common` not loadable).

## Phase 1/2 findings (next)
`common/index.js` top-level `require`s that must resolve to load ANY test:
`assert`, `fs`✓, `net`, `path`, `util`{inspect,getCallSites}, `worker_threads`{isMainThread},
`buffer`{atob,btoa}, `./tmpdir`. `tmpdir.js` adds: `child_process`{spawnSync}, `url`{pathToFileURL}.
Also `process.umask` (called at load) + `process.config.variables` — **both missing today**
(`process` exists, `arch`=arm64; add umask no-op + config object in `otter-runtime/process.rs`).

**ABI GAP (sanctioned to fix):** hosted modules build an *object* namespace
(`HostedModuleCtx::build` → `JsObject`), but `assert` must export a *callable*
(`assert(cond)` + `assert.strictEqual`). Need to let a hosted builtin set its export to a
function value (extend `HostedModule`/`commonjs` builtin branch), or build assert as a
native function with method props.

**Module port order (Phase 2):** assert (real, callable, ~legacy `crates-legacy/otter-nodejs/src/assert.rs` 837L) → path (real, pure, legacy `path.rs`) → util (inspect + getCallSites + types subset). Plus light stubs (net/worker_threads{isMainThread:true,Worker}/buffer{atob,btoa,Buffer}/url{pathToFileURL}/child_process{spawnSync…}) so `common` loads. Then re-run → real green numbers.
Legacy uses old ABI (`lodge!`, `RuntimeState`, `install_method`); port to active
`HostedModuleCtx` (`ctx.method`/`ctx.property`, `HostedNativeCall::dynamic`) like `fs.rs`.
