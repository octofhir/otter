# Otter architecture and build review

## Initial state

2026-11-12, HEAD `9c47049b721162ce850bf2427becec27d4797056`.
Before this work, `vendor/test262` was modified and `.claude/` and
`tests/test262/` were untracked; these were left untouched. No commits were made
in that review pass. AGENTS.md and ES_CONFORMANCE.md were read; the historical
99.98% was not used as evidence of current correctness. `fff` was unavailable,
so searches used `rg`.

macOS Darwin 25.6 ARM64, 12 CPUs, 36 GiB RAM, initially 85 GiB free disk.
Load average was 9.28 / 6.00 / 5.31: the host was busy. The missing pinned
`beta-2026-10-18` toolchain was installed: rustc/Cargo 1.100.0-beta.7, LLVM 23.1.4.
The toolchain file was unchanged. Downloads finished before measurements;
the offline attempt with installed 1.99.1 and missing jsonc-parser is not a baseline.

## Workspace from manifests, metadata, and source

An arrow means "depends on." The table lists the main public surfaces.

| Crate | Purpose and boundary |
|---|---|
| resource | ResourceAccount, limits, leases, snapshots |
| gc | Heap, GcSession, HandleScope, Local → resource |
| bytecode | BytecodeModule, verifier, opcode_schema, binary/disasm/dump → resource |
| syntax | SourceKind, with_program, SyntaxError → OXC |
| compiler | compile, CompiledModule, CompileOptions → syntax, bytecode, regex validation |
| regex | validate_utf16, Flags; runtime feature: Regex/MatchScratch/native tier → ICU |
| vm | Interpreter, Value, NativeCtx, builders, JIT contracts → gc, bytecode, resource, regex, macros |
| jit | Native compiler/artifacts → vm, bytecode; GC is a dev dependency only |
| runtime | Runtime/RuntimeBuilder, Otter, RuntimeHandle → vm, jit, compiler, syntax, gc, bytecode, resource, xml, http-client |
| http-client | Client/RequestBuilder/Response → hyper, rustls |
| xml | parse_bytes/parse_utf8, Node, sink; no dependencies |
| macros | HostClass/js_class and binding generators → syn/quote |
| modules | OtterModulesBuilderExt, serve/sql/kv/ffi/xml → runtime, macros, xml |
| node | Hosted Node modules/globals → runtime, vm; compiler in the build script |
| web | WebApiBuilderExt, wasm/crypto/fetch → runtime, macros; compiler in the build script |
| pm-manifest | PackageManifest, workspace discovery/settings → serde, Tokio |
| pm-lockfile | Lockfile, LockedPackage, import formats → serde JSON/YAML/TOML |
| pm-phantom | scan_project/extract_specifiers → manifest, syntax, OXC |
| pm | Installer, registry, resolution, policy → manifest, lockfile, http-client |
| cli | otter/otterx → runtime, modules, node, web, PM, syntax, bytecode |
| vm-codegen | Standalone gen-pow10, not a VM dependency |
| benchmark | Schema/recorder; engine, rss, warm-harness are feature gated |
| difftest | Process runner without an engine dependency |
| test262 | Runner/report/isolation → runtime, vm |
| node-compat | run(RunOptions), RunReport, CLI without an engine dependency |
| extension-template | Example → runtime, macros |

All crate names have the `otter-` prefix. Runtime joins the frontend and VM:
VM does not depend on compiler, GC does not know VM, and JIT consumes VM-owned
contracts. Runtime → Node/Web and macros → VM/GC appear only as dev dependencies.
Bytecode serves both runtime consumers and Node/Web host producers, so changes
trigger a broad rebuild. CLI has ten internal normal/build dependencies; runtime
has nine. Runtime exports advanced Value/NativeCtx APIs, but a leak through the
Send RuntimeHandle was not established. Generic monomorphization was not
established as a source of build cost either.

### Why the heavy dependencies remain

- CLI → web → Wasmtime/Cranelift: `wasm.rs` and `wasm/asm/instance.rs` actually
  compile modules. `wat` affects accepted input bytes, not just tooling.
- CLI → modules → rusqlite/bundled provides SQL/KV. Runtime/PM → http-client →
  rustls → aws-lc provides TLS for fetch/modules/registry. Removing these loses
  functionality.
- VM → ICU compiled_data/Temporal and compiler → regex → ICU properties provide
  Intl and Unicode RegExp support; these are not unused tables.
- JIT → yaxpeax serves `artifact/assembly*.rs`. Diagnostics are disabled during
  ordinary execution but ship in the default CLI and are always compiled.
- Bytecode serde/serde_json serve containers and `dump::to_json_pretty`;
  runtime JSON5/JSONC/YAML/TOML serve `data_modules.rs`.
- Criterion is a dev dependency. Node `primordials-codegen` and benchmark engine
  are optional. Broad Tokio features serve the full CLI. OXC/syn/clap/serde/dynasm
  proc macros are used. Host and target compiler units build separately.
- Duplicate syn 2/3, hashbrown 0.14–0.17, num-bigint 0.4/0.5, and wasm tooling
  have different consumers. CLI includes wasm-encoder 0.251 through Wasmtime and
  0.253 through asm/WAT. Metadata containing dev/target packages is not the set
  of units actually built.

## Changes implemented

1. Removed direct Node → flate2 and phantom → oxc_span dependencies: there were
   no uses; zlib uses libz_rs_sys. Both libraries remain transitively. Package and
   feature counts were unchanged by these removals (525 packages). Node normal
   dependencies went from 18 to 17; phantom from 10 to 9, including target-specific
   entries. Web → GC initially remained because generated code used `::otter_gc`;
   a later step below removed that edge.
2. At the user's additional request, upgraded OXC 0.154 → 0.158 without upgrading
   resolver 11.23 or the toolchain. Consumers now use JSStr; the old
   `U+FFFD + hex` decoder was removed. Host specifiers are checked rather than
   converted lossily. With explicit authorization, `TemplateSite.cooked` now stores
   UTF-16; binary IO, serde shape, accounting, compiler, and VM were updated.
   Old artifacts are incompatible.
3. Following the request to reduce workspace duplication, consolidated existing
   engine code: 110 identical `top_frame!` definitions in `dispatch_ops.rs` became
   one; two identical root carriers in `method_ops/mod.rs` became FastDispatchRoots;
   139 descriptors, symbol lookup, and the dense inventory in
   `native_abi/runtime_stubs.rs` now come from one table. All eight fields, names,
   ids, and ordering were preserved. `array::push` had a stale value when slab
   growth collected; it now uses the correct `push_with_roots`. Two indexed-store
   implementations were also consolidated. The existing remembered-array test
   reproduces the bug and now enables stress itself rather than relying on the
   environment. No allocations/dynamic dispatch were added to hit paths, and
   Value layout, barriers, safepoints, and deopt ABI were unchanged.

Tests were consolidated by responsibility, not removed to obtain passing results:
repeated OXC helper checks and property-key cases already covered by
`property_key_code_units` were removed. Primordials no longer assert magic counts
277/192; fixture membership, hashes, and idempotent regeneration remain checked.
Terminal restoration no longer depends on nonportable `/dev/fd` opening; order and
exactly-once behavior remain checked. `eval_value` uses an ES Promise job instead
of a Web global absent from bare Runtime. `jit_constructor_gc` no longer asserts
fixed first prologue bytes: a sloppy receiver guard can legally precede the
prologue. Exact code-object, call-entry offset, native-execution,
safepoint/return-site, and relocation assertions remain.

Math checks account for the existing `CheckNative` eager exit; replacements and
getters must still execute exactly once. A duplicate numeric-leaf matrix wrongly
promised no repeated Graph deopts: events show ShapeGuard followed by TypeMismatch
for a new receiver/callee. The shared differential matrix, leaf-hit counters,
evaluation order, and moving-GC checks remain; unique Set-miss and rope-search
cases moved into the shared matrix. The shape-directory test now checks two
effectful `LoadPropertyCached` operations rather than an obsolete commoned
`LoadShapedProperty`: accessor fallback must not replay or merge accesses.
Lowering code was unchanged for these fixture corrections.

Lint blockers were fixed without suppression. A large test-only CompileInput
payload was boxed; production representation was unchanged. An OXC helper's
unnecessary rejection of an unused attribute key was fixed by comparing JSStr
instead of requiring a UTF-8 name.

LOC counts cover the same Rust files under `crates/` and `examples/`, including
comments, tests, and two new OXC files, excluding vendor and parked crates.
The expanded pass changed 728,130 lines to 725,944: **−2,186 lines**.
`dispatch_ops.rs` lost 726 lines and the stub table lost 1,202. This was a first
pass, **not a 50% reduction of the repository**. Moving lines into a macro does
not itself accelerate builds; the benefit is one owner for repeated contracts.

## Measurements

Artifacts are under `/tmp/otter-architecture-review/`: metadata, trees,
`measurements.jsonl`, logs, and parser-probe. Old timings/CLI binaries were kept
in `saved-artifacts`; three review-owned build caches were removed (about 38 GiB).
The final `target-migration` was retained. User target directories/caches were
not removed.

CLI measurements used one `git archive HEAD` copy with the after patch, separate
empty target directories, dev profile, default features, default jobs, a populated
registry cache, and no page-cache reset. Stable Cargo timings were used, without
self-profile.

```sh
cargo metadata --locked --format-version 1
cargo tree --offline --locked -p otter-cli -e normal,build
cargo tree --offline --locked -p otter-cli -e features
cargo tree --offline --locked -p otter-cli -d
cargo build --offline --locked -p otter-cli --timings --target-dir "$TARGET"
cargo check --offline --locked -p otter-cli --timings --target-dir "$TARGET"
```

| Observation, seconds | HEAD | Two dependency edges removed | OXC + UTF-16 |
|---|---:|---:|---:|
| Clean build, one run | 115.38 | 109.51 | 134.19 |
| First check after build, one run | 66.64 | 75.82 | 90.53 |
| No-change check | 0.43 | 0.62 | 0.68 |
| VM edit, first sample | 8.97 | 9.43 | 9.80 |
| Bytecode edit, two samples | 9.71 / 9.98 | 12.62 / 9.42 | 13.09 / 9.25 |
| VM edit after restoring bytecode | 8.96 | 8.31 | 9.42 |

The trigger was a comment in VM/bytecode lib.rs in the copy only. The second VM
sample includes restored bytecode, so it is not a pure VM-only rebuild. ABI types
were not changed for measurement. The full matrix was limited by the cost of
multiple build trees. **No Rust build/check speedup was established.** One run
on a busy host proves neither an improvement nor a stable regression. This table
covers the OXC stage, not the subsequent engine deduplication.

Baseline timings: ICU experimental 49.10 s (frontend 48.20/codegen 0.90), VM 24.12
(14.76/9.36), compiler units 12.88 (3.65/9.23) and 13.97 (4.89/9.08), runtime 11.30
(4.97/6.33). aws-lc-sys build script took 42.34 s, zstd-sys 13.52, Cranelift 21.67.
The CLI unit's 6.95 s is not a linker-only measurement. Parallel units cannot be
summed. Unchanged ICU (58.63) and aws-lc (54.05) also slowed in the after run.

Parser-probe placed both versions in one release binary: 225 Node JS files /
2,842,685 bytes, SourceType::cjs + allow_return_outside_function, 20 passes/sample,
six alternating pairs with the first excluded. Diagnostics = 0. Medians were
0.240065 → 0.190775 s (**−20.5%**), ranges 0.237034–0.245666 /
0.188850–0.196062. This is a parser-only result, not a whole-Otter speedup.
The command in `parser-probe` was
`RUSTUP_TOOLCHAIN=beta-2026-10-18 cargo build --release --offline`, then the binary.
[OXC release](https://github.com/oxc-project/oxc/releases/tag/crates_v0.158.0).

Materializing 2,000 ASCII tagged sites in the dev interpreter, measured with
performance.now excluding startup/parser, using six alternating pairs minus
warmup: 109.97 → 109.83 ms. No meaningful runtime change was visible. UTF-16 uses
more memory for ASCII payloads: accounting charges capacity × 2. Contract details:
[frontend.md](../site/src/content/docs/engine/frontend.md).

A focused probe after engine deduplication performed 20,000 object pushes,
indexed overwrites, and reads, five passes after warmup. Before used the saved
OXC 0.158 CLI from before that pass; after used the current CLI. Both were
dev/default, six alternating pairs per tier with the first excluded, startup/parser
outside the timer, and equal checksums. Medians in milliseconds:

| Tier | Before | After |
|---|---:|---:|
| Interpreter | 1150.29 | 1149.67 |
| Template | 497.18 | 500.96 |
| Production tier policy | 121.95 | 122.98 |

Command: `python3 /tmp/otter-architecture-review/followup/array-probe.py`.
This is a debug-binary smoke benchmark on a busy host, not proof of no release
regression or a speedup claim. An optimized benchmark is still needed.

## Validation

Commands used a separate `CARGO_TARGET_DIR`, the pinned toolchain, and
offline/locked execution. Before engine deduplication, the default CLI build,
workspace all-target/all-feature check, compiler 145, syntax 13, bytecode 113,
phantom 20, Web 11 + 51 integration, zlib 2, and warm-harness 6 passed. Test262
had identical before/after path → outcome mappings on the local corpus:
template-literal 59, string 73, module-code 595 passed / 4 skipped. These are
727 passes, not full Test262 coverage. The new UTF-16 runtime test passed at
strides 1–16.

After engine deduplication,
`cargo test -p otter-vm -p otter-jit -p otter-runtime -p otter-node --lib --all-features --no-fail-fast`
passed VM 1305, JIT 349, runtime 277 + 1 ignored, and Node 29. IPC/FSEvents tests
ran with access to local system APIs: earlier sandbox failures were not production
bugs. After rebuilding the frontend, compiler 145, syntax 11, and bytecode 113
passed. All four remembered-array tests passed at every stride 1–16. Strict
workspace Clippy and fmt passed without suppression. The default CLI built;
the linker warned about a large `__eh_frame` in the dev binary and
proc-macro-error2 reported future incompatibility.

`bash scripts/gate.sh --quick all` passed with `OTTER_GC_STRESS=1`, covering GC,
constructors, Math, native leaves, and properties. Additional runtime tests passed:
eval 5, OXC 2, property-key 1, class-value 1, XML 6; OXC also passed strides 1–16.
Full logs for that pass are in `followup/`.

Old native-leaf and shape-directory failures were reproduced separately on
HEAD/OXC 0.154 in `target-baseline-followup`, without sharing workspace artifacts
with the current build. Math/constructor failures were also reproduced before
fixture corrections. This separates old-test corrections from the GC bug fix
and OXC migration.

Fmt and strict Clippy had failed on the earlier HEAD; those warnings were fixed,
not suppressed. An earlier differential run was interrupted after three minutes;
the full diagnostic run after OXC migration completed in 743.75 s, demonstrating
that it was not deadlocked. The full differential run after the GC fix passed:
198 programs, interpreter vs Template, including completion, console output, and
errors (`integration-final.log`). The full checked/release GC corpus and other
OS/architecture combinations were not checked.

## Risks and scope

- UTF-16 changes the single current bytecode/serde format without a compatibility
  layer; saved old modules must be rebuilt. ASCII cooked payloads are larger.
- Incorrect descriptors are the main stub-table risk. All 139 rows, eight fields,
  names, and order were compared; ABI and validation tests remain.
- The shared array-store path changes rooting during allocation. The regression,
  strides 1–16, and three runtime tiers were checked; the full optimized GC gate
  is still required.
- A 50% LOC reduction was not achieved. User functionality, vendor code, GC/JIT
  safety tests, and parked reference code were not removed to reach that number.

## Subsequent completed step: frontend without the RegExp matcher

`compiler/expr/literal.rs` uses `validate_utf16`, which calls the same
`parser::parse` as `Regex::compile_utf16`. Literal validation no longer lowers IR
or creates NativeTier; one UTF-16 buffer serves validation and the bytecode
constant. There is no second parser, new crate, or public AST.

`otter-regex` has a default-on `runtime` feature. The workspace compiler keeps
`unicode` without the matcher; VM explicitly enables `runtime`. Native matching
and the standard CLI retain their functionality. ICU casemap, memchr, smallvec,
and assembler dependencies belong to runtime; Unicode property tables remain
with the parser. The compiler-only normal/build graph shrank from **97 to 86
packages**, removing dynasm/dynasmrt, their support dependencies, ICU casemap/data,
and libc/memmap2. These libraries remain in the full CLI.

Parser-only builds use the same internal AST. Only its payload types have a
justified conditional `dead_code` allowance: validation does not read matcher
operands; runtime is still checked without that allowance. Lowering methods and
the ASCII bitmap do not compile without `runtime`. An existing
`--no-default-features` build blocker was also fixed: the `v` parser referenced
disabled `string_props`. The Unicode-off variant now rejects property escapes,
as its previous contract requires; the ordinary compiler always enables Unicode
and loses no grammar.

Measurements in `/tmp/otter-regex-boundary/measure.py` used separate before/after
copies, three alternating pairs of empty targets, pinned beta, dev,
offline/locked `cargo check -p otter-compiler --timings`, and warm registry/page
caches.

| Pair | Before, seconds | After, seconds |
|---|---:|---:|
| 0 | 58.18 | 24.10 |
| 1 | 36.08 | 21.08 |
| 2 | 15.42 | 14.70 |

The first pairs overlapped other builds. The large spread prevents an honest
percentage speedup claim. The smaller dependency graph and absence of matcher
construction during validation are established; these values are not full CLI
clean-build timings. The measured after snapshot predates the Unicode-off
configuration fix; the measured Unicode-on compiler configuration is functionally
unchanged.

The full `scripts/gate.sh` now checks compiler/parser-only separately: workspace
all-features cannot check that boundary because of feature unification. Default
matcher tests passed on ARM64 and as executable x86_64 macOS binaries (47 unit,
49 matching, 1 validation); x86 was not merely cross-checked. Compiler passed
145 tests; parser-only Unicode passed 13 unit + 1 validation. Minimal builds
without Unicode were also checked separately. Strict workspace, compiler-only,
and parser-only Clippy passed; default CLI and Node/Web build scripts built.

Test262 `language/literals/regexp`, four workers, unchanged 10 s timeout:
**242 pass / 4 timeout before and after**, with all 246 path → status mappings
matching. The four timeouts were `S7.8.5_A1.1_T2`, `A1.4_T2`, `A2.1_T2`, and
`A2.4_T2`: long code-unit enumeration loops using eval. They are not counted as
passes; elapsed milliseconds at timeout are not compared. The before runner was
built from an isolated snapshot immediately before this step, not the old
OXC 0.154 tree. Reports are `test262-before.json`, `test262-final.json`, and
`test262-comparison.json` under `/tmp/otter-regex-boundary`.

## Subsequent step: declarative extension dependencies

Without a direct `otter-gc` dependency, the extension example failed to build:
`couch!` and `holt!` produced E0433 in generated code. This was reproduced before
the fix by removing only the manifest dependency (`missing-dependency-before.log`).

`holt!`, `couch!`, and `js_class` inheritance resolvers now obtain `GcHeap` and
`RootScope` through the existing `otter_vm::__macro_support`; runtime already
exposes this contract to binding crates. Exactly two re-exports were added, not
the whole GC module. These are the same types with the same owner: no wrapper,
conversion, new trait, or runtime dispatch. Root slots, lifetimes, allocation
order, and barriers were unchanged. `__macro_support` remains a hidden generated
code contract, not a handwritten extension API. Explicit `Pelt`/`Groom` GC derives
retain their justified GC dependency.

Removed direct `otter-web → otter-gc` and `extension-template → otter-gc` edges.
The latter's normal dependencies are only runtime/macros; VM/GC are absent even
from its dev dependencies. The template builds with `#![forbid(unsafe_code)]`:
handwritten bindings must not use unsafe; internal root operations remain the
engine's responsibility. Existing template tests check classes, namespaces,
async bindings, and hosted modules; their execution was added to the full gate.
Web `File extends Blob` checks native inheritance through the same boundary.
Both binding/embedding guides were updated.

This removes a hidden consumer-manifest requirement, not build time: GC remains
a transitive runtime dependency. No new tests/crates were added; an existing real
consumer checks the regression. Logs: `/tmp/otter-macro-boundary/`.

Checks for this step: macros 14, extension-template 2, Web 72 passed / 1 ignored.
GC stress 1–16 ran ten existing checks per stride (160 passed): template, couch,
holt, and three Web scenarios, including native inheritance and async Blob reads.
Fetch tests required loopback sockets: sandbox EPERM was separated from engine
results, and the rerun with local sockets allowed passed. Default CLI, strict
workspace Clippy, formatting, and gate shell syntax were checked. The full closing
gate was not run; bytecode/Value layout, GC barriers, and JIT ABI were unchanged.

## Dynamic module-loading boundary

Two bugs were confirmed in `module_ops.rs`:

- `run_import_namespace_dynamic_operands` used lossy `coerce_to_string`;
  `evaluate_import_call_options` did the same for `type`. Requesting `./\ud800.js`
  actually loaded existing `./�.js`. Substitution happened before lookup in
  `module_resolution_target` and scheduling through `DynamicImportLoader::schedule`.
- Attribute type/support validation happened while enumerating properties.
  For example, `{ type: 42, get later() { throw marker; } }` was rejected with
  `TypeError` without invoking the getter.
  [EvaluateImportCall](https://tc39.es/ecma262/#sec-evaluate-import-call) completes
  `EnumerableOwnProperties` before validating attributes.

VM now uses exact `ToString`, retains the checked UTF-8 conversion result in
Rust-owned memory, and delays rejection until options are read. All attribute
values are first read in a handle scope: a subsequent getter or Proxy trap can
collect the heap. Errors go through the existing shared `CommittedValueError` →
rejected Promise handler. Nested `Result<Result<…>>` and a duplicate Promise
creation branch were removed. The production file became eight lines shorter.
No crates, dependencies, public APIs, dispatch layers, or GC/JIT ABI changes were
added. Collecting enumerable values adds a temporary handle list on the cold
`import(..., options)` path; this implements observable ordering, not a speedup.

`module_request_identity.rs` checks prelinked and on-demand loading, high/low lone
surrogates, actual U+FFFD, emoji, repeated namespace cache hits, exact coercion-call
counts, dynamic literals, JSON attributes through a collecting Proxy, thrown-object
precedence, and symbol/non-enumerable filtering. Both new tests failed on the
original implementation and then passed. Static-request checks in
`oxc_string_values.rs` remain. Lone surrogates are rejected by Otter's UTF-8 host
request API, not by JS strings generally. `import.meta.resolve` was unchanged
in this step.

| Check | Before | After |
|---|---|---|
| Two new runtime regressions | 2 failed | 2 passed |
| Test262 `language/expressions/dynamic-import` | 724 pass, 217 skip | 724 pass, 217 skip |
| Exact Test262 path → outcome | 941 rows | All matched |
| GC stress 1–16: identity, OXC strings, module error builders | Not run | 144 passed |

Test262 conditions matched: macOS ARM64, pinned beta-2026-10-18, dev profile,
production-tiered, four workers, 10 s timeout, one run per state. Skip policy was
unchanged; 217 skips are not validated behavior. Reports include runner SHA-256,
corpus revision, and semantic config. This checks correctness, not performance.
Build timings were not repeated: the dependency graph was unchanged and no build
speedup is claimed.

Checks: VM unit **1305 passed**; seven focused runtime suites **28 passed**;
dynamic-import capability and Web module suites **45 passed** (loopback sockets
allowed separately); default CLI and Test262 runner built; workspace Clippy
`--all-targets --all-features -- -D warnings`, formatting, and `git diff --check`
passed. The linker reported a large debug `__eh_frame`, and Cargo reported
future incompatibility in `proc-macro-error2`; these were not presented as errors
introduced by this change.

`jit_module_ops::synchronous_module_ops_match_interpreter_with_observable_effects`
failed on `reentrant > 0` **before and after**: semantic output matched, but the
fixture did not enter the required transition. The before failure was reproduced
in isolated `/tmp/otter-regex-boundary/source-before` with its own target;
`cmp` verified the test and original `module_ops.rs` were byte-identical.
The assertion was neither removed nor weakened. The full closing gate and runtime
on other architectures were not run; this step was not a fully passing workspace
gate.

Logs, original `module_ops.rs`, environment, Test262 reports/comparison, and per-run
stress results are under `/tmp/otter-module-boundary/`. Main commands, with a target
outside user caches:

```sh
export CARGO_TARGET_DIR=/tmp/otter-architecture-review/target-migration
cargo test -p otter-runtime --test module_request_identity --test oxc_string_values
cargo test -p otter-runtime --test module_dynamic_import_capability --test web_modules
cargo test -p otter-runtime --test gc_module_error_builders --test typed_module \
  --test module_cycle_and_lifecycle --test module_source_embedding --test module_top_level_lexical
cargo test -p otter-vm --lib
cargo build -p otter-cli -p otter-test262
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
$CARGO_TARGET_DIR/debug/otter-test262 run --filter language/expressions/dynamic-import \
  --jobs 4 --timeout 10000 --output /tmp/otter-module-boundary/test262-after.json
```

## Restored native module-read proof

The `jit_module_ops` failure was explained: `hot(80)` does not pay the compilation
cost under the current `TierWorkModel`. The original Template run had **0 compile
attempts, 0 generations, and 0 JIT events**. With `hot(1000)`, one Template OSR
generation for `hot` appeared and module transitions executed. Production policy
and VM/JIT code were unchanged; this was a stale fixture, not a backend bug.

The test now proves more than a positive aggregate counter:

- After normal warmup, it finds the installed/current/linked Template generation
  using `function_id` from an actual source trace.
- After full GC, it mutates the live export through a function in another module.
- The repeated call reads the new value and matches the interpreter result.
- The measured call has no interpreted `hot` opcode; the shared reentrant counter
  grows and the original `code_object_id` remains installed/current/linked.
- The interpreter oracle actually executes `LoadImportBinding`: the tracer cannot
  silently switch off and provide false evidence of native execution.

Native proof covers repeated binding reads. One-time namespace imports, star
re-exports, deferred evaluation, and `import.meta.resolve` retain observable
semantic checks, but this fixture does not prove their native execution.
A separate console-capture harness was removed; comparison uses owned completion.
This does not reduce total LOC: exact-execution assertions made the test larger.
No new tests, crates, or production abstractions were introduced.

Results: the original test changed from failed to passed; GC stress **1–16 passed**;
four module suites **12 passed**; `scripts/gate.sh --quick all` passed; workspace
Clippy `--all-targets --all-features -- -D warnings`, formatting, `bash -n`, and
diff checks passed. The full gate now separately runs `jit_module_ops`,
`module_request_identity`, and `gc_module_error_builders` (**8 passed** with that
same command).

Checked on macOS ARM64 with the existing pinned toolchain and dev profile.
Target: `/tmp/otter-architecture-review/target-migration`; logs and original fixture:
`/tmp/otter-jit-module-review/`. Build/runtime performance was neither changed nor
measured: only the test and gate changed. Full closing gate, other platforms, and
Test262 were not rerun. The previously found `jit_module_ops` failure is closed;
this does not establish a passing full workspace gate.

## Static import attributes and loading-error ownership

`module_attribute_type` previously found the first `type` and ignored other keys.
A static import with `extra` actually executed both dependency and entry. The
shared compiler/loader helper now validates every key before reducing the AST to
`(specifier, attr_type)`. Diagnostics point to the unsupported key's range.
Empty clauses, ordinary imports, `type`, and its escaped spelling are preserved.
No dependencies, DTOs, formats, or public APIs were added.

The distinction from
[InnerModuleLoading](https://tc39.es/ecma262/#sec-InnerModuleLoading) remains:
an unsupported static-request key produces `SyntaxError` during loading. Options
of `import()` itself still produce `TypeError` after reading every getter.
Loading a module with an invalid static request through `import()` rejects the
Promise with `SyntaxError`. This is one supported-key policy, not one error class.

A related bug was found in `handle::discover_remote_requests`: advisory prefetch
immediately returned a parse/request diagnostic from a literal dynamic target,
preventing JS from catching it. These diagnostics now remain the responsibility
of the existing `ModuleGraphBuilder`, which distinguishes required and optional
dynamic nodes. I/O, resource, and cancellation errors are not suppressed.
Changes are confined to the frontend/loading boundary; GC barriers and JIT ABI
are unchanged.

Public compiler entry-point checks cover supported clauses and exact error spans.
Runtime checks cover 32 static combinations, including re-exports, defer, missing
targets, and unknown keys before/after `type`; no execution of rejected graphs;
literal/on-demand imports; and both error classes. The async fixture checks the
final JS marker through a separate host call, not merely successful `run_module`
return. Strengthened regressions fail on the original helper/handle and pass
after the fix. The existing build fingerprint covers `otter-syntax/src`, so old
compiled-cache entries are not reused; no internal format version was added.

| Check | Before | After |
|---|---|---|
| Test262 `import-attributes` | 83 pass, 13 skip | 83 pass, 13 skip |
| Test262 `language/expressions/dynamic-import` | 724 pass, 217 skip | 724 pass, 217 skip |
| Exact path → outcome across both subsets | 972 unique paths | All matched |
| New static/async regressions | Failed | Passed |

Test262 used one run per configuration on macOS ARM64, dev profile,
production-tiered, four workers, 10 s timeout, and the unchanged skip policy.
The subsets overlap; their totals must not be added. Compiler **146**, syntax
**11**, focused runtime/module suites **69 passed**; GC stress 1–16 **192 passed**.
Default CLI/Test262 build, workspace Clippy all-targets/all-features, formatting,
and diff checks passed. Loopback sockets were allowed separately for loader tests.
Logs and comparison JSON: `/tmp/otter-import-attributes/`.

The dependency graph and hot paths are unchanged: no build/VM speedup is claimed,
and build timings were not repeated. Full workspace/GC closing gate and runtime
on other platforms were not run. Existing debug-linker and proc-macro-error2
future-incompatibility warnings remain.

## Phantom: parse goals and recovered ASTs

This continuation uses macOS ARM64 and the existing `beta-2026-10-18` toolchain.
The local checkout actually started at `7b8f80bda`, not `d3337f755`. At the user's
separate request, all 28 local commits were rebased onto fetched `origin/main`
(`d3337f755`); the post-rebase HEAD was `ebb80d228`. Three native-stub inventory
conflicts were resolved by moving additions/removal into the authoritative table.
All 141 descriptors, eight ABI/effect fields, diagnostic symbols, and dense ids
were checked against the previous branch. Backup:
`backup/pre-scanner-rebase-20261112`; the stash was retained. Working files were
restored and verified with SHA-256. The pre-existing unrelated changes in this
checkout are `docs/site/public/conformance/data.json` and `.github/banner.png`;
they remain byte-identical. `vendor/test262`, `.claude/`, and `tests/test262/`
were not touched. The scanner patch was verified before committing; no push/PR
was made.
Searches used the available `fff` service. The review is written in English,
including the translated historical sections above.

Actual callers and consequences:

- `scan_project` calls `extract_specifiers`; its only production consumer is CLI
  `otter doctor`. The scanner reads sources, compares specifiers with the manifest,
  and checks whether package.json exists in node_modules. It neither installs
  packages nor resolves a module graph nor executes JS. A false finding makes
  doctor unhealthy and changes its exit code.
- asm.js `translate` receives function source through the link/native hook, wraps
  it as an expression, and parses with the CommonJS goal. It checks diagnostics
  **before** visiting the AST; rejection leaves ordinary JS execution intact.
  No defect requiring a move to the runtime frontend or a contract change was
  established for this direct OXC use.
- Phantom violated its documented promise to skip unparseable files: OXC can
  return a nonempty AST together with recoverable diagnostics, and the visitor
  walked it unconditionally. No caller relies on this as a partial-result contract.

Public `scan_project` reproduction: a separate valid `import 'real'` file plus
one containing `import 'ghost'; const uninitialized;` (JS),
`import 'ghost'; throw\n0;` (TS), or `import 'ghost'; /(/;` (JS). All three
previously produced `[ghost, real]`; after the fix they produce `[real]`.
The original 20 Phantom tests passed on the updated base; the new regression
failed before the production fix.

The fix belongs in `otter-pm-phantom/src/extract.rs`. The visitor runs only after
a diagnostic-free parse; RegExp parsing is explicitly enabled, as in the runtime
frontend. Module is tried first, then CommonJS, which accepts Script syntax and
top-level `return` and `new.target`. A CommonJS fallback containing module syntax
is rejected: OXC accepts TS module declarations even under that goal, which would
otherwise hide a Module error caused by top-level return. The shared
`allow_return_outside_function` override for Module was removed. No APIs,
dependencies, traits, crates, or parser wrappers were added. A second parse can
occur only on the cold analysis path for source rejected by the Module parser;
no speedup is claimed.

Partial results remain a project-level policy: an unparseable file is skipped
entirely while other files continue to be scanned. Computed/type-only loads and
specifiers unrepresentable in UTF-8 are still omitted individually. High/low lone
surrogates, actual U+FFFD, and a surrogate pair are checked; no string substitution
is introduced. Occurrence ordering, guarded imports, and `scanned_files` as the
count of files read, including skipped files, are preserved. This advisory pass
does not select a loader goal from extension/package.json or replace compiler
semantic checks. The scanner patch does not change runtime loading, error classes,
observable effect order, or Value/GC/JIT contracts.

Checks for this continuation:

- Phantom **23**, syntax **11**, compiler **146**, VM **1305**, JIT **349** passed
  (`--lib --all-features --no-fail-fast`). The goal matrix covers a Script using
  `await.ready`, CommonJS return/new.target, Module top-level await, and TSX.
  The project regression also rejects JS/TS Modules with top-level return.
- Test262 `language/module-code`: **595 pass / 4 skip** before and after;
  `language/statements/return`: **16 pass** before and after. All **615 exact
  path → outcome** pairs, including skip reasons, match. Corpus:
  `be13516fb6441b950ba8a3df97eb34062c186972`, production-tiered, four workers,
  10 s timeout. Phantom is not a runner dependency; actual runner identity matches
  across both series and the final standalone runner build. This checks
  preservation of runtime semantics, not Phantom
  directly; the `scan_project` regression reproduces its defect.
- Default CLI/Test262 build, standalone Phantom Clippy
  `--all-targets --all-features -- -D warnings`, formatting, and diff checks passed.
  A CLI smoke check with `otter --json doctor --root <temp-project>` read seven
  files, reported only `[real, script]`, and returned the expected exit 1 for those
  two real findings; five malformed files added no findings. The standalone
  scanner declares no Cargo features and is checked separately from workspace
  feature unification. Existing debug-linker `__eh_frame` and proc-macro-error2
  future-incompatibility warnings remain.
- `OTTER_GC_STRESS=1 bash scripts/gate.sh --quick all` passed: GC, remembered
  arrays, both moving-GC constructor regressions, Math guards, explicit native
  calls, and named-property tests. This is focused regression coverage after the
  rebase, not the full optimized GC corpus or closing gate.
- Workspace Clippy `--all-targets --all-features -- -D warnings` stopped on **12
  pre-existing lint errors**: `double_must_use` in `abstract_ops.rs:250` and 11
  `needless_borrow` cases in `array_prototype.rs:5028–5396`. Both files are
  byte-identical to current HEAD and do not depend on Phantom. These are blockers
  in the updated base, not scanner-patch errors. Lints were not suppressed and
  VM code was not changed to address them.

All commands use the separate `/private/tmp/otter-scanner-target`; Cargo metadata
confirms that target_directory. `scripts/gate.sh --quick` does not override it:
child Cargo commands inherit `CARGO_TARGET_DIR`. User caches and the toolchain
were neither deleted nor upgraded; missing OXC 0.158 packages were fetched strictly
from the existing Cargo.lock. Logs and canonical Test262 reports:
`/private/tmp/otter-scanner-*.log` / `*.json`. Every before/after path/outcome pair
is retained in `/private/tmp/otter-scanner-comparison.json`.

Main validation commands for this continuation:

```sh
export CARGO_TARGET_DIR=/private/tmp/otter-scanner-target
cargo test --locked --offline -p otter-pm-phantom -p otter-syntax -p otter-compiler \
  -p otter-vm -p otter-jit --lib --all-features --no-fail-fast
cargo clippy --locked --offline -p otter-pm-phantom --all-targets --all-features -- -D warnings
# Currently blocked by the 12 existing VM lint errors listed above.
cargo clippy --locked --offline --workspace --all-targets --all-features -- -D warnings
cargo build --locked --offline -p otter-cli
cargo build --locked --offline -p otter-test262
OTTER_GC_STRESS=1 bash scripts/gate.sh --quick all
cargo fmt --all -- --check
$CARGO_TARGET_DIR/debug/otter-test262 run --filter language/module-code \
  --jobs 4 --timeout 10000 --output /private/tmp/otter-scanner-module-after.json
$CARGO_TARGET_DIR/debug/otter-test262 run --filter language/statements/return \
  --jobs 4 --timeout 10000 --output /private/tmp/otter-scanner-return-after.json
```

The full closing gate, full checked GC corpus at strides 1–16, and other platforms
were not run in this continuation. Historical ES_CONFORMANCE.md's 99.98% is not
current verification; this is not a claim that the whole workspace passes.

## VM lint blockers closed

Following authorization to commit verified slices, the Phantom fix and English
review were committed as `17ce5cf3`. The 12 VM lint errors listed above were then
fixed without suppressions: `StrictMatcher::scan` relies on `Result`'s existing
must-use contract, and the array callback driver passes its already-borrowed
execution context directly at the 11 affected call sites. Root ownership,
allocation order, callback ordering, and runtime ABI are unchanged.

Workspace Clippy `--locked --offline --workspace --all-targets --all-features --
-D warnings` now passes, as do formatting and diff checks. Validation reused the
dedicated `/private/tmp/otter-scanner-target`; the full closing gate remains
outstanding. No build or execution speedup is claimed for lint-only changes.

## FindViaPredicate observes Get without HasProperty

The next public-boundary reproduction found a semantic error shared by `find`,
`findIndex`, `findLast`, and `findLastIndex`. For a Proxy whose `has` returns false
and whose `get` supplies an indexed value, all four methods invoked `has` and
passed `undefined` to their predicate. Node v24.16.0 instead performed indexed Get in
forward/reverse order without invoking `has`. A throwing `has` also incorrectly
escaped from the search. The new runtime regression failed before the fix with
`Error: find invoked has`; the shared JS fixture passed in Node.

The owner is the existing `array_callback_native_dispatch` driver. The find family
now bypasses presence checks and reads every visited index with Get. Methods
that skip absent elements retain HasProperty before Get. The direct read of a
present own dense element is unchanged; this patch adds no allocation to that
path and does not change root carriers, barriers, Value layout, or JIT ABI.
Getter/callback errors still leave through the driver's existing cleanup path.

One fixture, `otter-difftest/corpus/array_find_property_protocol.js`, is shared
with a public runtime integration test. It checks all four search methods,
forward/reverse order, Proxy trap observability, callback receiver identity,
early-match return identity, abrupt Get completion, holes, own/inherited getters,
and preservation of HasProperty in seven other callback methods. Getter and
callback allocation checks retain object identities across moving collection.
The existing pathological-length findLast tests remain unchanged and pass;
the broader large-length approximation policy was not redesigned in this slice.

Validation:

- New runtime regression: failed before, passed after, under interpreter,
  Template and production tier policies. Both existing maximum-index tests pass.
  These are semantic policy comparisons, not a new assertion of hot-code native
  admission; existing native-admission assertions were not weakened.
- VM **1305** and JIT **349** unit tests pass with all features. Default CLI and
  Test262 runner builds, strict workspace Clippy, formatting and diff checks pass.
- Test262 `built-ins/Array/prototype/find` selects all four method families:
  **94 passed before and after**. All 94 exact path/outcome pairs and semantic
  configurations match. This subset did not detect the Proxy-has defect; the
  added regression covers that gap.
- The existing `scripts/dev/gc-stress.py` ran the shared fixture under interpreter,
  Template and production policies at every stride **1–16: 48/48 passed**.
  `results.json` reports `status: passed`, with unchanged executable/source hashes
  and exact completion/console agreement against the unstressed interpreter.

Artifacts: `/private/tmp/otter-find-{before,after,comparison}.json`,
`/private/tmp/otter-find-regression-{before,after}.log`,
`/private/tmp/otter-find-core-tests.log`, `/private/tmp/otter-find-clippy.log`, and
`/private/tmp/otter-find-stress/`. Builds use the same dedicated target. Only its
session-owned incremental data was removed after all Cargo jobs finished to make
room for closing builds; binaries, observations and user caches were retained.
The full closing gate is being run separately; this slice makes no performance
or full-workspace correctness claim.

## Native allocation-group recovery fixture restored

The first full-gate attempt stopped in `allocation_group_recovery`, before the
release/differential stages. An isolated checkout of pre-find commit `5e8b8bc54`
reproduced the same failure with its own target directory. Baseline compilation
disabled debug information and incremental storage to limit disk use; optimization,
debug assertions, default features and the pinned toolchain were unchanged.

Two fixture defects were confirmed:

1. The actual emitted comparison is `StrictEqual { negate: false, same_value:
   false }`; the fixture expected its older spelling without `same_value`.
   The comparison still reads the real AllocationGroup/AllocationProjection
   values and feeds the original branch. The test now checks the complete
   current mode, including `same_value: false`, and prints actual IR on failure.
2. After correcting only that expectation in the isolated baseline, preparation
   failed at `fill never advances prematurely`. LOS exhaustion can run real
   collections and promote the already-prepared young input. A diagnostic check
   confirmed its rooted offset changed while the fixture retained the old page
   identity. Preparing/aging the input after LOS pressure removes that stale
   assumption. Both zero-free-page assertions remain explicit.

The source program, normal-policy native admission, exact current generation,
live tagged group members, nonempty machine-code regions, typed ensure relocation,
canonical root homes, actual failed-minor collector records, first-source resume
PC, completed-prefix object identity and initialized-cell checks remain enforced.
No production collector, safepoint, native ABI or JIT lowering was changed.

The restored test passes, including **10/10 separate process runs**. Artifacts:
`/private/tmp/otter-allocation-group-{baseline,baseline-pressure,repro,pressure-repro,after}.log`
and `/private/tmp/otter-allocation-group-repeats.json`. The isolated checkout and
target are `/private/tmp/otter-engine-before-find` and
`/private/tmp/otter-engine-baseline-target`. Verification is on macOS ARM64;
x86_64 execution is not claimed. The full gate was restarted after this correction.

## Compile-fail diagnostics refreshed without weakening boundaries

The next full-gate attempt found 10 stale trybuild `.stderr` fixtures. Each actual
diagnostic was reviewed before regeneration. Cross-isolate Root/Weak rejection
still reports E0521 with the invariant brand; heap/handle/Frame/NativeCtx Send/Sync
and untraceable Pelt fields still report E0277 at their required bounds; hidden
TraceTable remains unavailable with E0425. Differences are pinned-rustc wording,
current internal type chains, and additional non-Send/Sync witnesses.

Only the 10 reviewed `.stderr` files changed. All 19 rejected Rust snippets and
the four test groups are unchanged; no expected failure was replaced with a
passing case or a different error class. After regeneration, normal trybuild
execution **without** `TRYBUILD=overwrite` passes all four groups. The audit diff
and normal validation logs are `/private/tmp/otter-compilefail-diff.txt` and
`/private/tmp/otter-compilefail-verified.log`; baseline investigation is retained
in `/private/tmp/otter-compilefail-baseline.log`.

## Gate executables follow Cargo output paths

The full gate previously built into the configured target but then invoked
`target/release/otter` for startup-image creation. Its differential runner also
defaulted to that relative path. With a dedicated target this could run a stale
binary or fail despite a successful fresh build.

The gate now reads the CLI executable from Cargo's compiler-artifact JSON,
fails if no executable was produced, and quotes that exact path for image
creation. The differential runner's default CLI is beside its own executable,
including the platform executable suffix; explicit `--otter` remains available.
Thus the standard gate keeps the selected target directory, target triple and
profile without a guessed repository-relative release path. The existing cost
ledger already obtains its executable from Cargo and needed no change.

Shell syntax checks pass. A real process smoke test with the dedicated target
ran all selected modes successfully and reported exactly
`/private/tmp/otter-scanner-target/debug/otter` as its CLI. The report is
`/private/tmp/otter-engine-target-path-smoke.json`. Two initial debug differential
runs of the allocation-heavy find fixture hit their 20 s GC-stride-1 watchdog
under concurrent stress; those reports remain failures, not passes. Separate
fixture stress validation completed 48/48 with 30 s caps. The path smoke uses a
small arithmetic corpus to isolate executable selection from GC-stress cost.
The release full gate retains its existing watchdog policy.

The isolated pre-find baseline also reproduces the same 10 compile-fail fixture
mismatches, confirming those diagnostic updates were unrelated to FindViaPredicate.
Release bytecode verifier/adversarial checks, 23 code-space tests, snapshot
boundary and four artifact-agreement tests pass.

## Closing differential verdict and retained failure evidence

The complete `scripts/gate.sh` invocation at engine commit `36c2c7344` exited
**1** at its differential stage: **79 passed / 120 failed**, across all **199**
committed corpus paths and six process modes per path. Its 20 s per-process
watchdog was unchanged. All 199 interpreter oracles completed. There were
**233 timed-out candidate observations**: 120 at GC stride 1, 70 at stride 4,
and 43 at stride 16. These remain failed observations; no cap was increased and
no corpus program was removed. The full checked GC corpus at strides 1–16 is
still outstanding.

One of those 120 cases, `parameter_entry_widening.js`, also had a completed
normal-tier semantic divergence. After calling `label(1, "!")`, the later numeric
loop incorrectly begins concatenating strings. Interpreter and Template print
the expected `800060000` final numeric total. Three additional normal-tier runs
reproduce the corruption, with different concatenation start points. This is a
separate optimizing-tier correctness issue, not a timeout classification.

An isolated pre-find `5e8b8bc54` release build reproduces the same error **3/3**.
Both builds' unpatched Cargo CLI artifacts were compared without startup images:
interpreter/Template outputs agree, and all three production outputs match
byte-for-byte between baseline and current. The baseline disables debug
information and incremental storage only; optimization, default features,
toolchain and lockfile are unchanged. This is correctness evidence, not a
performance comparison. Baseline `arithmetic.js` also exceeds the same 20 s
stride-1/slot-verification cap without an image. That sample confirms an inherited
stress-timeout problem; the other timed-out paths were not individually baselined.

The baseline's separately patched startup-image executable instead exits with
SIGSEGV before user source in all policies. Those failed observations are retained
and excluded from the JIT comparison; the raw executable succeeds. This additional
image/profile interaction needs isolation rather than a claimed passing baseline
for the shipped restore path.

A ten-line `label` reproduction and captured JIT artifacts narrow the arithmetic
defect to `graph::builder::visit_binary`: string-concat feedback permits a
`NumberToString` on one proved numeric operand while the other operand is still
unknown. That operand can be numeric too. The captured inline IR converts a
numeric sum to a string before `PrimitiveAdd`, whose second input later has
Int32 Select representation. Historical concat feedback does not prove current
string operands. No lowering, allocation contract, deopt ABI or native-admission
assertion was changed during this investigation.

The newly added `array_find_property_protocol.js` passes all six release modes,
including GC strides 1, 4 and 16, with exact `ok:find-property-protocol` output.
The other completed candidate observations agree with their interpreter oracle;
this is tier agreement, not an external conformance or full-workspace claim.

Evidence is retained in `/private/tmp/otter-engine-full-gate-closing.log`,
`/private/tmp/otter-engine-full-difftest.json`,
`/private/tmp/otter-engine-full-difftest-failures.json`, and
`/private/tmp/otter-parameter-current-repeats.json`,
`/private/tmp/otter-parameter-raw-comparison.json`, and
`/private/tmp/otter-arithmetic-baseline-raw-stress.json`. Reduced source and native
artifacts: `/private/tmp/otter-parameter-minimal.js`,
`/private/tmp/otter-parameter-minimal-events.json` (48 events, no truncation), and
`/private/tmp/otter-parameter-minimal-artifacts/`. The failed image run is retained
in `/private/tmp/otter-parameter-baseline-observations.json`.

The original gate did not reach its kernel ledger. A separate run encountered
the sandbox's denied `time -l` read of `kern.clockrate`; it remains a failed
measurement run in `/private/tmp/otter-engine-kernel-ledger.log`. Repeating the
unchanged script outside that sandbox passed **30/30** kernel/tier combinations
(15 kernels, Template/production, 20 samples plus 8 warmup invocations) and exited
0: `/private/tmp/otter-engine-kernel-ledger-unsandboxed.log`. This neither changes
the failed differential verdict nor establishes a before/after speedup.

The failed run also exposed a reporting defect: `set -e` stopped the gate before
it printed its temporary JSON path or parsed the failure summary. The gate now
prints that path before execution and retains a nonzero runner status while
rendering the report. The runner reports each completed case's PASS/FAIL,
elapsed time and timeout count to stderr; stdout remains one JSON report.
Comparison logic, GC verification, mode selection, watchdog and exit policy are
unchanged. Real process smoke tests check a passing arithmetic source and a
deliberate 1 ms watchdog failure: the latter retains valid JSON and exit 1,
reporting six timed-out processes rather than agreement. Formatting, shell
syntax, strict workspace all-target/all-feature Clippy and the runner's three
existing rejection/agreement tests pass. Artifacts:
`/private/tmp/otter-difftest-observability-{pass,timeout}.{json,stderr}`.

## Large FindViaPredicate ranges: pre-fix reproduction

Read-only follow-up probes found that the callback driver's pathological-length
approximation also changes `find*` semantics. These observations predate the
full-range correction and validation recorded below; the earlier Get/HasProperty
correction alone did not fix them.

With `length: 2 ** 26`, a Proxy containing index `0: 'first'` and a predicate that
immediately returns true should let `find` return `'first'` and `findIndex` return
`0`. Node v24.16.0 does so without metadata traps. All three Otter policies instead
throw `TypeError: Array.prototype callback: Object.getPrototypeOf: cannot operate
on a value of type proxy`. The driver's preliminary prototype enumeration runs
before the first indexed Get, even though FindViaPredicate needs no enumeration.

For an ordinary object with the same length, an element at `2 ** 26 - 2`, and a
predicate that matches its second invocation, Node returns that element (or
index `67108862`) after two calls. Otter's `findLast` returns `undefined` and
`findLastIndex` returns `-1`, both after only one call. The `visit_all && reverse`
branch constructs a singleton index list and silently truncates the search.

The source and exact observations are retained at
`/private/tmp/otter-find-large-probe.js`,
`/private/tmp/otter-find-large-{actual,reference}.txt`, and
`/private/tmp/otter-find-large-observations.json`. The last report checks
interpreter, Template and production policies against the same debug executable.
These original artifacts retain the public CLI failures; the passing shared
regression below covers these conditions.

The correction below keeps the existing callback driver and verifies native and
compiled predicate cancellation/budgets before accepting its complete lazy range.
The other callback methods' sparse approximation still needs its own scope and
validation rather than an incidental broad rewrite.

## Confirmed next tasks

1. **P1: resolve cold full-suite failures and finish the closing gate.** Guarded
   Add's exact after-change Test262 comparison and final strict Clippy repeat
   now pass, as recorded below. Classify and correct the broader test failures
   against baseline without weakening their assertions; a successful targeted
   subset does not close this task.
   The `jit_global_access` abort was traced to its fixture stack footprint and
   repaired below. Its four exposed guard/call/foreign-realm proof failures are
   diagnosed and repaired in the final section with native/GC assertions preserved.
2. **P1: diagnose the GC-stress watchdog failures.** Keep exact failed paths and
   limits. Re-run the full closing gate after correcting the causes, then the
   separate full checked GC corpus at strides 1–16. Do not turn timeouts into
   passing agreement or hide them by filtering the corpus.
3. **P1: isolate baseline startup-image SIGSEGV.** Compare raw/restored launches
   under the same release profile before attributing the failure to image or
   debug-information settings. Current shipped-image conformance is not inferred
   from the raw baseline comparison.
4. **P1: audit the other bounded Array drivers.** The generic callback range
   correction is recorded below. `flatMap` still clamps its input range, and
   other uses of `MAX_ARRAY_LIKE_PROBE_LEN` need their own property-order,
   materialization and interruption audit. Preserve the generic callback driver's
   complete ordered HasProperty/Get and Find's Get-without-HasProperty behavior.
5. **P1: repeat build measurements on an idle host.** At least three alternating
   before/after clean-build pairs, a separate linking measurement, and a restorative
   build between incremental triggers. Do not promise a 50% LOC reduction without
   identifying the responsibilities actually removed and the checks preserved.

## Optimizing Add: guard current String operands before conversion

Starting from `2e10b49f2`, the new public runtime regression failed before the
production change: after `labelAfterConcat(1, "!")`, the later numeric loop
returned `[800020000,["2!",3.5,8],"string",false]` under production tiering.
Interpreter and Template returned the expected `"number",true`. The failure
reproduces with synchronous compilation and normal tier admission.

The graph builder now inserts `CheckString` on the unknown counterpart before
emitting `NumberToString` for a proved numeric Add operand. String-only historical
feedback still selects the specialization but does not establish that proof.
When both operands are already proved numeric, or neither receives the early
conversion, their existing PrimitiveAdd paths remain intact. The number-string
cache fast path is retained.

The guard has one tagged input, an eager source state and no allocation, runtime
call or safepoint. Both AArch64 and x86-64 check cell/null bits before reading the
immutable String type tag. A miss resumes the original Add before conversion.
Its value-only effect, redundancy and loop-motion contracts follow the existing
scalar guards. No Value layout, barrier, safepoint format, deopt ABI, runtime-stub
inventory, dependency or contributor API changed.

Completed checks before the requested Cargo cleanup:

- Both public runtime regressions pass under interpreter, Template and
  production policies. They cover numeric addition after concat feedback,
  boolean/null/undefined/double inputs, BigInt/Symbol TypeErrors, ordered and
  throwing `Symbol.toPrimitive`, exact thrown identity and lone UTF-16 units.
  The main regression also requires actual optimizing entries.
- The graph regression checks both operand orders, a current String guard before
  cached conversion, the original eager source state and native PrimitiveAdd.
  All **350 JIT unit tests** pass.
- All **three native primitive-string integration tests** pass on macOS ARM64
  and on x86-64 under Rosetta. The new proof performs **64 measured native probes
  per architecture** (two operand orders, cache hit/miss, strides 1–16). Exact
  installed generations, emitted guard/conversion source PCs, no interpreter
  replay, real minor collection/slot updates, moved inputs, exact UTF-16 results
  and subsequent full-GC survival are asserted. A numeric pair produces one
  exact-generation TypeMismatch at the original Add, no allocating probe, one
  interpreter Add and result 42. Existing native/GC assertions are unchanged.
- A release CLI with its own startup image reproduces the old production failure
  **3/3** and the corrected numeric result **3/3**; interpreter/Template agree.
  `/private/tmp/otter-add-feedback-release-comparison.json` retains every outcome.
- Strict workspace all-target/all-feature Clippy passed with the production and
  native-test changes. The later public coercion test compiled and passed;
  a final cold all-target Clippy repeat remains pending. Formatting and diff
  checks pass. Test262 addition baseline is **48/48 passed** with exact rows in
  `/private/tmp/otter-add-feedback-test262-before.json`; after-change comparison
  remains pending rather than being inferred from the regressions.

Primary logs: `/private/tmp/otter-add-feedback-{before,after,runtime,builder,native,jit-unit,clippy}.log`
and `/private/tmp/otter-add-feedback-x86-native.log`. The executed x86-64 test
binary and checksum are retained at `/private/tmp/otter-add-feedback-x86-native-test`.

The broader VM/runtime/JIT test attempt did not complete: parallel linking ran
out of disk space. A serial follow-up completed ten two-target batches before
the user's cleanup request interrupted it. It observed failures in browser event
and timer tests (`queueMicrotask` unavailable), captured eval chunk accounting
(`1` rather than `2` evictions), and exact Known-linkage checks in
`jit_actual_only_calls`. These failures are retained and not all baseline-classified;
none of those tests or assertions was edited. Full results and individual logs
are under `/private/tmp/otter-add-feedback-core-batches/`. A later cold full test
and checked build were explicitly stopped for disk cleanup. Neither invocation
counts as a passing gate. The earlier complete differential gate remains failed.

The user explicitly requested Cargo and broader disk cleanup. All active task
builds were stopped. `cargo clean` removed the temporary engine targets and the
verified Cargo targets of `fml-rs` and `ccda-fhir`; sources, corpora and reports
were preserved. Local Docker cleanup removed dangling images and unused build
cache only: all 10 containers and 64 volumes remained. Available space reached
**57 GiB**. Heavy builds were stopped at that checkpoint. Subsequent work uses
one temporary target with both dev/test debug information disabled and assertions enabled,
rather than recreating several debug-info-heavy target trees. Baseline binaries
and hashes survive separately in `/private/tmp/otter-retained-baseline-binaries/`;
the former baseline build cache and finished cross target were removed.

## Core microtask global: one bootstrap owner

At `f0c88e886`, default `Runtime::builder().build()` exposed no
`queueMicrotask`. The Web installer owned its only callable even though the VM
already owned callback admission, roots, realm/context capture and FIFO draining.
This broke the public runtime boundary without a Web extension: browser event
turn and embedder timer tests failed with `ReferenceError`. The new public
regressions also failed before the production change, including the check that
Web installation preserves the core callable's identity.

The VM now installs one CORE `queueMicrotask` through the existing static
MethodSpec/ObjectBuilder/bootstrap flow. Its native body delegates to the same
`NativeCtx::queue_microtask`; the Web installer no longer replaces it. There is
no new queue, runtime interface, dependency, dynamic dispatch, manual rooting,
Value representation or compiled-code ABI. The obsolete comment claiming that
direct calls use a dedicated microtask opcode was removed: current direct and
indirect calls resolve the ordinary callable.

The shared `core_microtask_queue.js` fixture and public runtime tests check the
descriptor/name/arity/non-constructor contract, synchronous callable validation
without coercion, ignored extra arguments, undefined callback receiver, aliases,
`.call`, Proxy and bound callbacks, nested jobs and FIFO agreement with Promise
reactions. Allocations before the checkpoint retain queued callbacks and their
receivers through stress collections. Web installation is separately required
to preserve exact function identity. Existing event-turn, embedder timer and
all ten microtask-ordering tests pass. No existing assertion was weakened.

The bootstrap measurement changed from 146 to 147 reported native functions,
3448 to 3451 allocations and 367152 to 367416 bytes. The existing allocation
and byte caps remain unchanged; only the expected native-function inventory
increased by one. All four bootstrap tests pass. Core availability is documented
in the event-loop and type-package docs; the existing `@types/node` dependency
already supplies the signature, so no duplicate TypeScript declaration was added.

Validation at this checkpoint uses the locked offline dependency graph and one
`/private/tmp/otter-engine-compact-target`, with dev/test debug information and
incremental compilation disabled while assertions remain enabled:

- Default-feature CLI, Test262 and difftest builds pass. Strict workspace
  all-target/all-feature Clippy passes with `-D warnings`; formatting and diff
  checks pass.
- Guarded Add follow-up: both public runtime regressions pass again. Test262
  addition remains **48/48** with the same exact paths and outcomes, corpus
  commit, semantic configuration and target. The comparison is retained in
  `/private/tmp/otter-continuation-addition-comparison.json`; runner executable
  hashes are recorded separately rather than required to match across builds.
- The Promise `prototype/then` subset passes **75/75**. This is an after-change
  observation, not a fabricated before/after comparison or a whole-suite claim.
- `gc-stress.py` passes **48/48** process observations: interpreter, Template and
  production policies at every stride 1–16, with GC verification enabled. Its
  final `status: passed`, exact executable/source hashes, commands, console and
  completion comparisons are retained in
  `/private/tmp/otter-core-microtask-gc-final/results.json`. This focused debug
  build run does not replace the outstanding full checked-profile GC corpus.
- VM/runtime checks without default features pass. The exact default-global
  snapshot was updated by adding only `queueMicrotask w.c function`; all three
  snapshot tests and both final public queue regressions pass. Strict Clippy
  passes again after that fixture update.
- All **48 Web integration tests** pass, including indirect microtask callers,
  rejection notifications, the common-API ledger and WebAssembly interactions.
  `/private/tmp/otter-core-queue-web-tests.log` retains the full observation.

Primary logs: `/private/tmp/otter-core-queue-{before,after}.log`,
`/private/tmp/otter-core-queue-bootstrap-{before,after}.log`,
`/private/tmp/otter-continuation-{baseline-tests,core-full,clippy,tools-build}.log`.
The broader test run and process-isolated GC/differential checks are tracked
separately below; targeted successes do not make the full closing gate green.

### Completed broad checks and remaining failures

The cold `cargo test --locked --offline --all-features --no-fail-fast
-p otter-vm -p otter-runtime -p otter-jit` invocation completed, including docs.
All **1305 VM unit tests**, **350 JIT unit tests**, the native primitive-string
integration proofs and all four VM compile-fail groups pass. The in-process
Template/interpreter comparison passes over all **201 corpus programs**, including
the new queue fixture. Its unoptimized run took 830.76 seconds; this is a single
validation observation, not a build or execution speedup measurement.

The broad invocation reports **44 failed targets**, including the default-global
inventory expectation that was then corrected by one row and retested
successfully. No second full passing invocation is claimed. Browser event,
embedder timer, microtask ordering and both Add regressions pass in this broad
run. Full native-admission, exact-generation, GC and no-replay assertions remain
intact in the other tests, including the ones that fail.

Two redirect tests fail to bind local sockets inside the sandbox and both pass
outside it. IPC also initially fails with `Operation not permitted`; its
unsandboxed repeat exposes a separate charge assertion (`6` rather than `3`).
The exact outputs are retained in
`/private/tmp/otter-continuation-unsandboxed-permissions.json`. A retained old raw
CLI and the current interpreter both return `false` in the `arguments.callee.caller`
case whose fixture expects `true`; that independent comparison is retained at
`/private/tmp/otter-continuation-caller-baseline.json`.

A supplementary unoptimized process differential invocation with
`--gc-strides 0` was interrupted after 19 progress records when fixed 20-second
caps produced timeouts. It produced no completed JSON gate report and supplies
no passing-gate claim. Its partial paths and status remain in
`/private/tmp/otter-continuation-difftest-no-forced-gc{.log,-status.json}`. It does
not replace the previous optimized full differential failure report or the
focused passing queue stress matrix.

The remaining **43 targets** were then executed against `f0c88e886` production
sources in the same target, locked graph, feature set and debug/assertion
profiles. The three changed production files were saved, compared with the
commit, substituted for the run and restored byte-for-byte afterward; the new
`globals` module is not declared by that baseline. No other source, fixture or
user file was replaced. **40 targets fail on baseline**, including the same
`global_epoch_dictionary_and_descriptor_misses_preserve_current_native_body`
stack-overflow thread and SIGABRT in `jit_global_access`. The other failures
include native-admission/GC proofs, captured eval eviction, snapshot ownership,
runtime trybuild diagnostics and its missing `ValueWindow` import, dependency
boundary checks and trace/audit fixtures. Their assertions remain unchanged.

The baseline's runtime unit, redirect and dynamic HTTP module targets pass
outside the sandbox. The restored patch passes those targets under the same
permissions, together with all affected queue/timer/snapshot/Add tests:
**333 passed, 0 failed, 1 pre-existing ignored**. The exact compared target/test
identities and outcomes, including the abort, are retained in
`/private/tmp/otter-queue-baseline-audit/comparison.json`; there are no remaining
outcome differences among the originally differing identities after the
equivalent-permission repeats. This compares statuses, not process-local native
addresses or complete artifact bytes.

IPC and compile-cache assertions also pass **10/10 standalone repetitions each**
on both baseline and restored source. Their earlier failures are not declared
fixed: notably the first unsandboxed IPC repeat really observed the doubled
charge. Scheduling/environment sensitivity still needs a separate diagnosis.
The baseline command, source hashes, exact restoration and full log are recorded
in `/private/tmp/otter-queue-baseline-audit/{audit.json,otter-runtime.log}`;
the restored checks are in `/private/tmp/otter-core-queue-restored-final.log`.
The previous failed full optimized differential gate and the outstanding full
checked GC matrix remain open. The workspace is not declared green.

Final restored-source workspace Clippy and formatting checks pass again. All
seven tested source/fixture hashes and both unrelated user-file hashes match
their saved values. After all test/compiler processes stopped, `cargo clean`
removed **36.5 GiB** from this task's temporary target; free space is **52 GiB**
at the closing observation. Neither the temporary nor default repository target
remains. The three exact CLI/Test262/difftest executables (317 MiB total) and
their SHA-256 manifest survive at
`/private/tmp/otter-core-queue-retained-tools/`; the CLI passes the queue fixture
in all three policies after relocation. Test reports and baseline evidence are
preserved outside the cleaned target. No dependency/toolchain update, unrelated
cache cleanup or performance claim accompanies this change.

## Restore the global-binding native proof on the default test stack

At `5207e788e`, the exact dictionary/epoch/descriptor regression again aborts
with SIGABRT. LLDB stops in the stack probe of
`Interpreter::with_string_heap_cap`, through the first `Harness::new` and
`RuntimeBuilder::build`, before JavaScript runs. This is fixture stack pressure,
not demonstrated JavaScript recursion or a native binding fallback failure.
The test's ARM64 unoptimized prologue reserves 735712 bytes, including saved
registers: eight by-value harness locals retain large runtime/oracle pairs and
leave too little stack for normal construction. The simplified public CLI
getter probe passes under interpreter, Template and production policies.

An investigative 8-MiB-stack run proceeds to another stale fixture assumption:
the emitted `write_barrier` has descriptor id 25, while the fixture expects 26
(currently `copy_context_alloc`). The committed correction does not enlarge
the test stack. Each independent scenario constructs and drops its pair inside
a scoped helper, retaining a fresh epoch/layout for every miss. The test's own
measured ARM64 frame is now 96 bytes. The barrier check uses the canonical
`STUB_WRITE_BARRIER.id`, keeps its exact name and call-count checks, and also
requires the mutating two-value leaf signature. Every original source program,
expected result, cold-transition count, exact-generation, no-deopt/no-replay and
GC assertion remains in place. No runtime allocation, VM layout or ABI changes.

The previously aborting regression passes **10/10** standalone repetitions on
the default Rust test stack, with all eight scenarios and nine probes each.
The full target now completes **4 passed /
4 failed**, rather than aborting the process. Passing coverage includes lexical
cells, own loop OSR and the existing exact moving-child checks at strides 1–16.
The still-failing tests are intentionally not relaxed:

- `heap_only_frozen_ordinary_globals_decline_current_native_strict_and_sloppy_stores`
  rejects a dictionary discriminator in its ordinary-guard region.
- `frozen_dictionary_globals_decline_current_native_strict_and_sloppy_stores`
  and `sealed_dictionary_globals_retire_descriptor_guards_and_keep_writable_stores`
  do not find the expected private-JS observer edge.
- `foreign_realms::foreign_callable_binding_misses_keep_defining_realm_and_moving_actuals`
  observes interpreter dispatch of the foreign body during its native probe.

Strict workspace all-target/all-feature Clippy passes. Formatting/diff checks
pass; the focused native target was run on macOS ARM64 with assertions enabled,
both dev/test debug information disabled, and no `RUST_MIN_STACK` override. This
is restored test coverage, not an engine stack-safety or performance claim, nor
a passing full gate. Primary evidence is retained at
`/private/tmp/otter-binding-{before,after-fixture,suite,clippy}.log` and
`/private/tmp/otter-binding-{before-backtrace,before-disassembly,after-disassembly}.txt`.

## Complete lazy FindViaPredicate ranges with bounded native work

Starting from `389af315e`, the new public runtime regressions reproduce the
large-range semantic and work-budget failures. The shared program also fails
through the retained CLI in interpreter, Template and production modes. Forward
Proxy searches perform an unwanted preliminary prototype walk; reverse searches
silently stop after their first absent index. A rejecting work budget never
crosses its limit when the reverse walk returns this incorrect early result.
The existing ten work-budget configuration tests pass before the change.

The callback driver now selects a full lazy ascending/descending range for all
four `find*` methods, independently of length. It needs no length-sized vector,
key enumeration or preliminary prototype traversal. The unreachable Find
prefix/singleton approximations are removed; the sparse iterator for other callback methods
retains its existing behavior. Length remains snapshotted once, indexed values
are read live with Get, absent indices are visited, and callbacks can terminate
immediately. The clamped `Number.MAX_VALUE` first-match regressions remain intact.
Dense callback admission also rejects an unmatched callback before cloning the
receiver's values, so a dense Find immediate match stays lazy as well. Existing
matched numeric Map/Filter/Reduce fast paths retain their guards and behavior.

Each Find index attempt now enters a small VM-owned native iteration checkpoint
before Get. It observes the interrupt flag even under an observing budget,
charges one unit of real loop work without incrementing native-call counts, and
uses the existing Reject/Yield policy owner. This keeps a straight-line native
or generated predicate from bypassing loop control. The enclosing driver still
releases its existing iteration anchors on every error. There is no new public
runtime interface, dependency, trait, GC/root layout, safepoint, deopt ABI or
invalidation policy; the default checkpoint introduces no allocation or dynamic dispatch.
Configured work limits can now reject a long Find walk according to its actual
index work rather than a truncated approximation.

The shared `array_find_large_range.js` covers both directions at `2**26` and
`Number.MAX_SAFE_INTEGER`, absent indices, exact callback receiver/arguments,
single length capture, mutation of length and the next element, forbidden
HasProperty/key/prototype traps, exact abrupt-Get identity, length-error priority,
lone UTF-16 units and moving Array/Proxy receivers across allocating callbacks.
The public runtime tests also exercise rejecting budgets, cooperative yields,
FIFO microtask timing, cancellation and runtime/GC reuse after abrupt termination.
Independent indexed getter sentinels bound broken budget/cancellation tests.

Native predicate admission uses normal source warmup and exact captured code
objects. Template entry counts agree with actual predicate effects; the Graph
backend has no entry counter, so its proof uses the selected installed generation,
observable effect count, independently validated interpreter tracer identity and
absence of deopt events. Both policies retain their generation, linkage and
private-JS entry with zero active frames after the error; no interpreter subject
execution can satisfy the native budget test. This does not add counters to the
engine or force native admission.

Current validation retains exact commands/reports outside the one temporary
`/private/tmp/otter-binding-target`. Default CLI/Test262 builds pass; Test262
`built-ins/Array/prototype/find` remains **94/94**, with identical path/outcome
rows, corpus commit, semantic configuration and target in
`/private/tmp/otter-find-range-test262-comparison.json`. The exact-source CLI
comparison records three failing before policies and three passing after
policies at `/private/tmp/otter-find-range-cli-comparison.json`.

All **1305 VM** and **350 JIT unit tests** pass. The four new public tests pass
on macOS ARM64 and x86_64 under Rosetta, with explicit Template/Graph admission
and no test-stack override. The ARM64 affected runtime matrix passes **20/20**,
including existing max-index, property protocol, work-budget and timeout tests.
Strict workspace all-target/all-feature Clippy, formatting/diff checks, default
CLI/Test262 builds and VM/runtime no-default-feature builds pass. These checks
use assertions with dev/test debug information disabled. The quick GC family
passes at stress stride 1 and inherits the explicit Cargo target; it creates no default
repository target. The shared large-range program passes the existing
process-isolated GC driver **48/48** at strides 1–16 in all three policies, with
GC verification and exact console/completion agreement. Its final
`status: passed`, executable/source hashes and per-run commands are retained at
`/private/tmp/otter-find-range-gc-final/results.json`. The final source hashes,
commands and report paths are retained in
`/private/tmp/otter-find-range-{final-inputs,validation}.json`; verified CLI,
Test262 and ARM64/x86_64 regression executables are retained under
`/private/tmp/otter-find-range-retained-tools` independently of the Cargo target.
After validation, cleaning only that temporary target removed **7.2 GiB** and
left **51 GiB** available. The repository's default target was not created;
user caches and unrelated working-tree files were preserved.

This focused matrix does not replace the outstanding full checked GC corpus or
the failed full closing gate. The four exposed global-binding proof failures
were still open at this checkpoint; their later repair is recorded below. Other
callback methods' large-range approximations are deliberately not claimed fixed.

## Complete the current-generation global-binding regression proof

At `b22f55c4b`, a fresh compact debug build reproduces **4 passed / 4 failed**
in `jit_global_access`. Additional failure diagnostics identify the foreign
exit as `AllocationSiteDecided` with `Recompile` in the optimizing tier, at
the return-array allocation after the first measured collection. Its result
already agrees with the interpreter. The correction preserves this legitimate
placement policy and prepares the fixture before selecting its measured code.

Three independent fixture assumptions had drifted from their runtime owners:

- A cold named read no longer migrates an ordinary dictionary receiver:
  `make_prototypes_fast` intentionally prepares its prototypes. The ordinary
  global setup now makes a no-argument named call to its existing observer,
  earning migration through normal method feedback before either writer is
  admitted. It still performs no deletion, padding, prototype publication or
  descriptor override. Exact ordinary guard emission remains mandatory.
- The known native observer enters the canonical `STUB_JIT_CALL_NATIVE`, rather
  than the generic classifier. The assertion now requires that exact id/name,
  the private-JS signature, its own source operation and the actual machine
  call. It still requires exactly one observer edge, with no extra cold calls.
- The foreign return-array placement was undecided at admission. A preparation
  collection and another ordinary source warmup settle it. Artifacts from both
  warmup turns are joined to the one current installed body; an older linked
  generation or another function's bundle cannot satisfy the proof.

The measured foreign source, fresh young argument, strict existence snapshot,
source-realm values, real moving collection and exact generation before/after
remain unchanged. Its probe still forbids interpreter subject steps, deopt,
compilation, inlining and replay. The parent harness also now requires
`current_entry` at admission and every probe, and an active own native frame at
the actual observer. No production source, placement/invalidation policy,
admission threshold, ABI, GC assertion or test-stack setting changes.

The target passes **8/8** on macOS ARM64 and x86_64 under Rosetta, including its
existing exact young-child matrix at strides 1–16. The adjacent four public
large-Find regressions also pass on ARM64. Each architecture passes **10/10**
standalone repetitions of the whole eight-test target on the default test
stack, with normal tier policy and assertions enabled. Test262 global-code
remains **195/195**, with identical exact paths/outcomes and semantic
configuration. These are repeated runs of the same retained runtime binary;
this fixture-only change makes no conformance or engine-performance claim.
Strict workspace all-target/all-feature Clippy, formatting/diff checks and
VM/runtime no-default-feature builds pass, using one separate Cargo target
with dev/test debug information disabled and assertions enabled.
Broader validation still observes three failures in the unchanged
`jit_known_native_calls` target. On ARM64, two
Native-entry count assertions see two edges instead of one, and `mixed_get`
finds no expected committed Get edge. These match the prior ARM64 baseline
audit at `/private/tmp/otter-queue-baseline-audit/otter-runtime.log`. On x86_64,
the first two cases stop at the constructor's generated-JS return-site
assertion; `mixed_get` has the same missing-Get-edge assertion. Their source
hashes match HEAD and none of these neighboring assertions is relaxed.

Primary reports are retained at
`/private/tmp/otter-global-proof-{before,host-final,x86-final}.log`; the final source
hashes and unchanged neighboring fixture hashes are in
`/private/tmp/otter-global-proof-inputs.json`. Per-repeat observations and exact
Test262 identities are in `/private/tmp/otter-global-proof-repeat-{arm,x86}.json`
and `/private/tmp/otter-global-proof-test262-comparison.json`; both verified
test executables survive under `/private/tmp/otter-global-proof-retained-tests`.
Commands and both passing/failing target outcomes are summarized separately in
`/private/tmp/otter-global-proof-validation.json`.
Cleaning only this task's temporary Cargo target removed **4.9 GiB** and left
**50 GiB** available. Reports and verified executables remain; the default
repository target was not created and unrelated working-tree hashes match.
The full closing gate and full checked GC corpus remain outstanding.

## Complete generic Array callback ranges and control hole attempts

At `f5ba3f495`, public CLI probes reproduce erroneous preliminary prototype
traversal for `some`, `every`, `forEach`, `map`, `filter`, `reduce` and
`reduceRight` at length `2**26`. A Proxy whose first element already satisfies
`some` throws `TypeError` instead of returning true. The new public runtime
target fails all three tests before the change: ordered live property access,
work rejection over holes and cancellation. A plain large object also returns
normally without exhausting its enforcing budget, because the old sparse
approximation never attempts its absent indices.

The existing callback driver now walks its captured range lazily in the
specified direction at every length. The present-index BTreeSet/Vec snapshot
and its custom iterator are removed. HasProperty remains live before Get for
methods that skip holes; Find still performs Get without HasProperty. Their
bounded property-protocol owner and rooted receiver/callback/accumulator/output
anchors remain unchanged. Every attempted index now enters the existing
native iteration checkpoint, including a hole that invokes no JS callback.
Species creation, length capture, callback validation, early completion and
error ordering retain their existing owners. Matched numeric callback fast
paths retain their admission and behavior. `flatMap` remains a separate,
unfixed bounded driver; this is not a claim that every Array range is corrected.

The shared corpus covers all seven methods at `2**26` and
`Number.MAX_SAFE_INTEGER`: exact HasProperty/Get order, skipped holes, live
deletion/addition after length mutation, forward/reverse reducers, callback
receiver/arguments/accumulator, custom species and exact result writes,
early/abrupt completion, lone UTF-16 units and allocating callbacks. Ordinary
object cases also prove that additions beyond the initial index set are seen.
Public budget tests prepare their input in a separate turn and then require an
uncatchable resource rejection while visiting holes. Cancellation must stop
before a second observable HasProperty entry; every regression has a bounded
safety failure independent of the budget/interrupt it is testing.

The existing exact-generation predicate proof now also covers `reduce` and
`reduceRight`. Each earns normal Template/Graph admission, runs a present first
element and must stop in its subsequent hole range without deopt, interpreter
fallback or replacement generation. The callback's effects and tracer identity
still independently prove actual native execution. No GC layout, barrier,
safepoint, deopt ABI, invalidation rule, dependency or admission threshold changes.

The ARM64 affected runtime matrix passes **33/33**; all **1305 VM** and
**350 JIT unit tests** pass. The two large-range targets pass **7/7** on x86_64
under Rosetta. Test262 covers **1605 exact paths** across the seven methods:
**1601 passed / 4 skipped** before and after, with no changed path/outcome,
semantic configuration, corpus commit or target. The newly reproduced cases
are coverage beyond that existing subset, not a claimed aggregate pass-rate gain.
Reports and inputs are retained under `/private/tmp/otter-callback-range-*`;
the exact comparison is `otter-callback-range-test262-comparison.json`.

The shared new corpus passes the existing process-isolated GC driver **48/48**
at strides 1–16 in interpreter, Template and production modes. Its final
`status: passed`, exact binary/source hashes and commands are retained in
`/private/tmp/otter-callback-range-gc/results.json`. The standalone difftest
also passes both this corpus and `array_find_large_range` under normal policies
and GC stride 16, comparing complete output/diagnostics with zero timeouts.
That report is `/private/tmp/otter-callback-range-difftest.json`. The frozen-source
CLI comparison records three failures before and three passes after in
`/private/tmp/otter-callback-range-cli-comparison.json`. Strict workspace
all-target/all-feature Clippy, formatting/diff checks and VM/runtime
no-default-feature builds pass. The existing quick GC family also passes at
stride 1 and inherits this same explicit Cargo target. Final commands and
report paths are collected in `/private/tmp/otter-callback-range-validation.json`.

The working Cargo cache is intentionally retained at
`/private/tmp/otter-engine-target` for subsequent iterations. Commands use
`CARGO_INCREMENTAL=0`, `CARGO_BUILD_JOBS=2`, and both
`CARGO_PROFILE_DEV_DEBUG=0` / `CARGO_PROFILE_TEST_DEBUG=0`, retaining assertions.
User caches and unrelated working-tree files are preserved. The full closing
gate and full checked GC corpus remain open.

## Capture actual shared JIT routine owners and restore native proofs

The fresh `jit_known_native_calls` baseline fails all three tests. Two ARM64
assertions count a literal-pool address word as another call; the same symbol
has one load instruction and one data relocation. Constructor warmup with
`Array` also admits intrinsic allocation rather than the Native entry the
fixture is intended to exercise. The return-array placement decision can then
retire the measured body at its first collection. These are fixture assumptions,
not evidence that the native entry or placement policy is broken.

The committed Get proof exposes a separate production diagnostics gap. Its
source operation now transfers to an isolate-shared load routine in another
physical mapping. Function-only artifacts contain the transfer symbol but omit
that mapping's code and relocations. Searching an obsolete embedded-probe region
cannot prove its actual canonical C miss. A public CLI capture at `f58d7402e`
reproduces the missing shared bytes without requiring a runtime semantic error.

Placed routine owners now retain their symbolic relocation records from the
original emission. Explicit artifact capture borrows those actual owners and
adds `routines.json` only when the function references shared code. Each
referenced physical object occurs once, with all its entry aliases, local
offsets, runtime address range, exact bytes, portable normalized bytes and the
ordinary validated relocation document. Unrelated cached objects are excluded;
missing required owners fail the capture invariant. Source and GC records stay
with the calling function. No routine is re-emitted for diagnostics, and shared
C misses acquire no synthetic JavaScript return sites.

The native fixtures now distinguish instruction relocations from pool data and
still require the exact stub id/signature, machine CALL, source region and root
record. Boolean construction earns the existing Native plan before the Array
probe; a preparation collection and normal warmup settle allocation placement
before admission. Current-entry checks are strengthened. Exact installed
generation, actual native execution, young motion, real OOM, no replay, no deopt
and no interpreter fallback assertions remain mandatory. No admission threshold,
GC layout, barrier, safepoint, deopt ABI or invalidation policy changes.

The affected runtime matrix passes **21/21 on ARM64** and **20/20 on x86_64 under
Rosetta**: artifacts, global accesses, all three known-native cases and two new
shared-owner tests. The new tests retain the same physical mapping through GC
and later captures, exclude a genuinely populated unrelated store routine,
validate owned payloads after runtime destruction, and compare portable identity
between distinct simultaneously live mappings on both targets. All **1305 VM**
and **350 JIT unit tests** pass, including the updated explicit manifest inventory.
Strict workspace all-target/all-feature Clippy, formatting/diff checks,
VM/runtime no-default-feature checks and CLI/Test262/difftest builds pass.

Public CLI comparison preserves the exact source and function's normalized
machine code before/after, while the new shared payload closes the missing-code
gap. Shared normalized code and symbolic records also match across independent
CLI processes. Exact Test262 `built-ins/Reflect/get/` coverage remains **11/11**,
with identical paths/outcomes, corpus, target and semantic configuration.
Existing differential corpora pass **5/5** in interpreter, Template, production
and GC stride 16 modes, including megamorphic loads, nested property reentry,
Proxy receiver roots and both large Array-range corpora; no process times out.

The original baseline is `/private/tmp/otter-known-native-before.log`.
Final evidence lives under `/private/tmp/otter-routine-capture-*`:
`{host,x86}-complete.log`,
`units-final.log`, `clippy-final.log`, `features.log`, `build.jsonl`,
`cli-comparison.json`, `test262-comparison.json`, both differential JSON reports,
and the final input/validation manifests. The pre-change CLI/runner binaries
are retained with hashes in `otter-routine-capture-before-tools/manifest.json`.

This adds cold relocation metadata once per placed shared object, including
when artifact capture is disabled. Disabled capture still copies no routine
bytes, normalizes no code and builds no artifact JSON. No execution-time or
compile-time speed improvement is claimed; metadata footprint is not benchmarked.
The current artifact contract is updated in place, without a compatibility
format, extra crate or dependency. The persistent Cargo target is kept at
`/private/tmp/otter-engine-target`; unrelated files and user caches are preserved.
The full closing gate and full checked GC corpus remain outstanding. The
flattening slice below resolves `flat`/`flatMap`; other bounded Array drivers
still require separate audits.

## Bound native flattening depth and control the complete source range

At `cf3171dd7`, the public CLI aborts with a Rust stack overflow on
`const a = []; a[0] = a; a.flat(Infinity)`. The isolated process exits by
SIGABRT even with a two-second runtime timeout. A second public probe places a
throwing getter at index `2**25` on an object of length `2**25 + 1`.
`flatMap` returns normally without visiting it, because its caller silently
clamps the captured range. `flat` and recursive child arrays have the same
clamp. Existing Test262 coverage nevertheless passes all 44 selected paths.

The shared native walk also bypasses rejecting work budgets over holes. A
bounded public-runtime regression reaches its independent getter sentinel at
10000 and executes its source catch/later effects instead of rejecting the
turn. The existing callback-budget regression reproduces the same failure
when extended to `flatMap`; the seven previously corrected methods still pass.
These observations precede the production changes.

`array_prototype/flatten.rs` now owns one iterative FlattenIntoArray driver.
Its explicit stack preserves depth-first order, captured lengths, live
HasProperty/Get, top-level-only mapping and CreateDataPropertyOrThrow. The
three range clamps are removed. Every attempted index, including a nested
hole, enters the existing native work/cancellation checkpoint before property
access. Terminal budget/control errors retain the committed fatal domain.
Length capture, mapper validation, depth coercion and species creation retain
their existing order and owners.

The target, mapper, callback receiver, current element and source frames use
scoped GC handles. A frame's source slot is reused at that depth and cleared
when it completes; root storage grows with peak nesting, not source element
count. Four frame records fit inline. At most 512 active source frames are
retained; deeper nonempty input raises a catchable `RangeError`, after its
nested length read. This deliberate implementation limit bounds native frame
storage and stops cyclic infinite-depth input without using the Rust stack.
The safe-integer target-index check and actual property/call error dispositions
remain intact. No dependency, Value layout, barrier, safepoint, deopt ABI,
invalidation policy or native admission threshold changes.

The new shared corpus checks exact source/child/species/write order at large
lengths, holes and live deletion/addition after length mutation, callback
arguments and `this`, inherited getters, revoked-proxy IsArray behavior,
one-level mapping, UTF-16 units, deep sibling reuse, abrupt identity and
allocating target traps. The existing callback corpus now includes `flatMap`.
Public resource regressions cover top-level/nested holes, rejection bypassing
source catch handlers, repeated cyclic failure, valid 512-frame input and
catchable 513-frame exhaustion, plus yield/microtask ordering. The existing
predicate fixture now also requires normal Template/Graph admission for
`flatMap`, exact retained generation, actual native effects, no deopt and no
interpreter fallback; its assertions are preserved.

The affected runtime matrix passes **16/16 on ARM64 and 16/16 on x86_64 under
Rosetta**. All **1305 VM** and **350 JIT unit tests** pass, including existing
collecting species-consumer checks. Strict workspace all-target/all-feature
Clippy, formatting/diff checks, VM/runtime no-default-feature checks and
CLI/Test262/difftest builds pass. The existing quick GC family passes at stride
1 and inherits the explicit Cargo target. The standalone differential runner
passes all three selected corpora in interpreter, Template, production and GC
stride 16 modes, with zero timeouts. Both changed JS corpora also pass on
installed Node **v24.16.0**.

Exact Test262 coverage is unchanged: **19/19 `flat`** and **25/25 `flatMap`**,
with identical per-path outcomes, corpus, target, assertions and semantic
configuration before/after. The original cyclic CLI probe now produces a
catchable RangeError in all three execution policies; the full-range probe
reaches the getter at `2**25`. The long unoptimized range probe is semantic
evidence only, not a comparable speed measurement.

Reports are retained under `/private/tmp/otter-flatten-*`, including
`cyclic-{before,after}.json`, `boundary-{before,after}.json`,
`budget-before.log`, `control-before.log`, `{host-complete,x86-final}.log`,
`units.log`, `clippy-final.log`, `features.log`, `build-final.jsonl`,
`test262-comparison.json`, `difftest.json` and `reference.json`. The first
x86 test attempt exposed an accidental Rust test block embedded in a new JS
raw-string fixture; the fixture was corrected before both final passing runs.
Pre-change executables and hashes remain in `otter-flatten-before-tools`.

The new flatten corpus also passes the existing process-isolated GC driver
**48/48** at strides 1–16 in interpreter, Template and production modes.
`/private/tmp/otter-flatten-gc/results.json` reports `status: passed`, with
exact unchanged binary/source hashes and per-run output retained alongside
`runs.jsonl`. Final inputs and commands/results are collected in
`/private/tmp/otter-flatten-{inputs,validation}.json`.

The persistent `/private/tmp/otter-engine-target` is retained with the same
debug/assertion and parallelism settings, occupying **8.4 GiB** at closure.
Unrelated files and user caches are preserved. The full closing gate and full
checked GC corpus remain open; the other bounded Array materialization/probe
drivers are still separate work. No performance improvement is claimed.

## Preserve live join reads across reentrant coercion and close its roots

At `092f9dfe9`, a public two-element example reproduces a common-path semantic
error in all three execution policies:
`const a = [{toString(){a[1] = 'after'; return 'first';}}, 'before']; a.join('|')`
returns `first|before`, while Node returns `first|after`. Join's dense snapshot
admits object elements before any conversion. An object's ToString can then
replace, delete or redefine a later element, but the snapshot still supplies
its old value. A plain dense array's initial data layout proves no indexed
getter effect; it does not prove that subsequent conversions cannot reenter.

A focused VM regression independently reproduces an abrupt-root ownership
error: a native indexed getter actually performs full GC and throws its exact
captured marker. The old join returns with six handles where the enclosing
scope previously had five, because its manual JSON root pop is skipped by `?`.
The failing baseline is `otter-join-rooting-before-final.log`; the public shared
corpus also fails before the production change.

`array_prototype/join.rs` now owns the existing join driver and Latin-1 helper.
Dense snapshots admit only primitive own data elements. Object and callable
elements, holes, accessors and proxies use the existing live Get/ToString
sequence. This retains primitive Latin-1 copying and the existing rooted
primitive snapshot during allocating number conversions. It does not introduce
a second property or coercion implementation.

One handle scope owns the receiver and separator before ToObject, the boxed
receiver during length/separator coercions, and every subsequent receiver read.
All early returns and errors close that extent. The manual JSON root pair and
outer raw-value RootScope are removed; existing primitive-vector rooting is
preserved. Length capture, separator conversion, per-element coercion,
UTF-16 output and original property/call error dispositions retain their order.
No Value layout, barrier, safepoint, deopt ABI, invalidation policy, native
admission threshold, crate or dependency changes.

The shared corpus covers dense replacement, installing a later getter,
shrinking length, inherited indices after prototype mutation, nested joins,
ordered Proxy Gets without HasProperty/enumeration, separator effects,
primitive boxing, exact lone UTF-16 units, Latin-1 and wide primitive cases,
and abrupt identity/order at length, separator, Get and element conversion.
It passes on installed Node **v24.16.0**. The focused VM proof checks indexed
getter and element-coercion failures three times each, with actual full
collection, fresh young input, changed source offset, exact original thrown
value and unchanged handle-arena/iteration-anchor extents. Its assertions stay
outside the native callback, and it passes on both architectures.

The affected runtime matrix passes **17/17 on ARM64 and 17/17 on x86_64 under
Rosetta**. All **1306 VM** and **350 JIT unit tests** pass. Neighboring exact
native generation, no-fallback, real OOM and GC assertions remain intact.
Strict workspace all-target/all-feature Clippy, formatting/diff checks,
VM/runtime no-default-feature checks and CLI/Test262/difftest builds pass.
The existing quick GC family passes at stride 1 and honors the same explicit
Cargo target. The standalone differential runner passes all three selected
join/callback/flatten corpora under interpreter, Template, production and GC
stride 16, with zero timeouts.

Test262 `built-ins/Array/prototype/join/` remains **23/23** before/after, with
identical exact paths/outcomes, corpus, target, assertions and semantic
configuration. The frozen-source public CLI example returns `first|after` in
all three execution policies after the fix. The pending-separator primitive
boxing control also passed on the pre-change binary at stress stride 1; it is
preservation coverage, not a separately claimed reproduced bug.

Evidence is retained under `/private/tmp/otter-join-*`, including
`live-{before,after}.json`, `rooting-before-final.log`, `runtime-before.log`,
`rooting-x86-final.log`, `{host,x86}-final.log`, `units-final.log`,
`clippy-final.log`, `features-final.log`, `build-final.jsonl`,
`test262-comparison.json`, `difftest.json`, `reference.json` and `quick-gc.log`.
Pre-change executables and their hashes remain in `otter-join-before-tools`.

The new join corpus passes the existing process-isolated GC driver **48/48**
at strides 1–16 in interpreter, Template and production modes.
`/private/tmp/otter-join-gc/results.json` reports `status: passed`, with
unchanged executable/source hashes and every observation retained alongside
`runs.jsonl`. Final source/binary identities and validation commands/results
are retained in `/private/tmp/otter-join-{inputs,validation}.json`.

The existing join materialization ceiling and scratch/output admission are
unchanged and still require their own full-range/resource-policy correction.
This slice does not claim complete large-length join conformance or a speed
improvement. The full closing gate and full checked GC corpus remain open.
The persistent `/private/tmp/otter-engine-target` and unrelated user files are
preserved. The retained target occupies **9.0 GiB** at closure.

## Root fill and reverse before reentrant preparation

At `a4b3c05ad`, forced collection in a public receiver's `length` getter
reproduces a preparation-time use-after-move. With
`const o = {get length(){gc(); return 0;}}`, `Array.prototype.reverse.call(o)`
returns a different identity. A two-element reverse, generic fill with a
collecting length getter, and dense fill with a collecting start coercion all
abort on the forwarded-cell diagnostic. These isolated CLI probes use
`--interpreter --expose-gc`; their complete stderr and exits are preserved.

The drivers opened their receiver handles after reading length. Fill also
carried its pending value and bound inputs through length/start/end coercion
before creating handles. The native argument window can keep the objects alive
and move them correctly while these detached Rust copies remain stale. The
short reverse return bypassed even the later receiver handle.

`array_prototype/mutate.rs` now owns the two existing drivers. Reverse opens
its receiver handle before ToObject and LengthOfArrayLike and resolves that
handle on the short return. Fill roots receiver, payload and optional start/end
operands before any reentrant preparation. Each coercion reads its input from
the current slot; in particular, end is resolved after start completes. The
boxed receiver replaces the receiver slot, and dense admission reads the
current array after all preparation. Existing per-pair reverse handles, dense
fill admission, prototype proof, reservation/writes, error disposition and
mutation bounds remain intact. No Value layout, barrier, safepoint, deopt ABI,
invalidation, native admission threshold, dependency or crate changes.

The focused VM proof exercises the actual native builtins for seven cases:
empty/pair reverse; empty fill; collecting length getter, length-number, start
and end coercion. Source and payload are freshly young before entry. Exactly
one selected child callback performs full collection, both offsets change,
result/source/payload aliases remain exact, and only the caller's returned
Local is retained. The start/end cases also prove ordinary dense admission
before the call. Callback bodies contain no assertions. This proof passes on
ARM64 and x86_64 under Rosetta.

The shared corpus covers preparation order, captured length after shrink,
pending operands, empty-range coercion, UTF-16 payloads, collecting indexed
operations, all four reverse presence branches, holes, Proxy traps without
enumeration/preliminary prototype traversal, and exact abrupt fill values/order.
It passes on Node v24.16.0. All four forced-GC public reproducers return true
after the change in interpreter, Template and production policies (**12/12**).

The affected runtime matrix passes **18/18 per architecture**. All **1307 VM**
and **350 JIT unit tests** pass; neighboring exact native-generation, no-fallback,
real-OOM and GC assertions are unchanged. Strict workspace all-target/all-feature
Clippy, formatting/diff checks, VM/runtime no-default-feature checks and
CLI/Test262/difftest builds pass. The existing quick GC family passes at stride
1 and honors the explicit persistent target. Exact Test262 coverage remains
**22/22 fill + 18/18 reverse**, with identical paths/outcomes, pinned corpus,
target, assertions and semantic configuration before/after.

Reports live under `/private/tmp/otter-mutate-*`: `public-before.json`,
`public-after.json`, `rooting.log`, `rooting-x86.log`, `{host,x86}.log`,
`units.log`, `clippy.log`, `features.log`, `build-final.jsonl`,
`test262-comparison.json`, `reference.json` and `quick-gc.log`. Verified
pre-change CLI/runner executables remain in `otter-mutate-before-tools`.

The new mutation corpus passes the existing process-isolated GC driver
**48/48** at strides 1–16 across interpreter, Template and production modes.
`/private/tmp/otter-mutate-gc/results.json` reports `status: passed`, with
unchanged binary/source hashes and a complete `runs.jsonl` journal. The
standalone differential runner passes the mutation, join and flatten corpora
in interpreter, Template, production and GC stride 16 modes, with zero timeouts.
Final identities and commands/results are collected in
`/private/tmp/otter-mutate-{inputs,validation}.json`.

Full-range mutation clamps and loop-control admission remain separate work.
This slice fixes preparation/root ownership, without claiming complete large
fill/reverse conformance or a speed gain. The full closing gate and full
checked GC corpus remain open. The persistent Cargo target and unrelated user
files are preserved.

## Prove writable length before a growing dense fill

After the preparation fix, a separate public repro still exposes an invalid
bulk hit: a fill-bound `valueOf` shrinks a three-element array to length one
and makes length non-writable. Direct fill grows it back to three without
throwing. Calling the same builtin through an empty-handler Proxy correctly
commits index zero and throws at index one. The new runtime target fails
before the guard correction; the direct CLI result is `[false,3,9,9]` in
all three execution policies, versus the generic control's `[true,1,9,null]`.

This contract follows the standard's ordered throwing
[fill Set operations](https://tc39.es/ecma262/multipage/indexed-collections.html#sec-array.prototype.fill)
and [Array index definition rule](https://tc39.es/ecma262/multipage/ordinary-and-exotic-objects-behaviours.html#sec-array-exotic-objects-defineownproperty-p-desc):
extending a non-writable length must fail. Earlier valid indexed writes remain
committed. Installed Node v24.16.0 / V8 13.6.233.17-node.49 reproduces the bad
plain-array result too; its generic Proxy path agrees with the standard. The
plain V8 fast path is therefore not treated as the oracle for these cases.
Both observations and exact executable versions are retained.

The existing `array::can_fast_fill_dense_range` is the shared receiver-state
admission owner used by bulk mutation, callback outputs and native leaf
consumers. It now declines a range whose end exceeds live length when the
sidecar marks length non-writable. Writes inside current length, including
holes, retain admission. Prototype, extensibility, density, accessor and indexed
descriptor checks remain intact. A declined fill uses its already-computed
bounds and ordinary Set path; coercions are not replayed and no eager error
replaces the valid prefix. There is no new allocation, dynamic dispatch,
carrier, dependency or ABI change in the guard.

The shared regression covers shrink-and-lock in start and end conversion,
zero/one-element retained prefixes, direct and Proxy receivers, exact live
length/descriptor preservation, legal in-length writes on dense and holey
read-only-length arrays, and the existing non-writable-element prefix case.
The standalone public result becomes `[true,1,9,null]` in every execution
policy. The pre-change CLI/runner and hashes are preserved in
`/private/tmp/otter-fill-length-before-tools`.

The affected runtime matrix passes **24/24 per architecture**, including the
new target, preparation/GC, join/flatten/callback, canonical map, exotic Array
and exact known-native/shared-routine proofs. All **1307 VM** and **350 JIT unit
tests** pass; the actual collecting preparation proof also passes separately
on x86_64. Strict workspace all-target/all-feature Clippy, formatting/diff
checks, VM/runtime no-default-feature checks and CLI/Test262/difftest builds
pass. The existing quick GC family passes at stride 1 using the same Cargo
target. The differential runner passes the new guard, preparation and shared
spare-push corpora in interpreter, Template, production and GC stride 16 modes,
with zero timeouts. Native admission and GC assertions are unchanged.

Exact Test262 fill/reverse coverage remains **40/40** relative to the previous
slice, with unchanged paths/outcomes, corpus, target and semantic configuration.
Reports are retained under `/private/tmp/otter-fill-length-*`:
`public-{before,after}.json`, `proxy-before.json`, `before.log`, `after.log`,
`{host,x86}.log`, `units.log`, `rooting-x86.log`, `clippy.log`, `features.log`,
`build.jsonl`, `test262-comparison.json`, `reference.json`, `difftest.json` and
`quick-gc.log`. The reference report records the installed V8 failure rather
than presenting it as a passing cross-engine gate.

The new guard corpus also passes the existing process-isolated GC driver
**48/48**, at strides 1–16 across interpreter, Template and production modes.
`/private/tmp/otter-fill-length-gc/results.json` reports `status: passed` and
zero failures; its executable/source hashes match the final build and corpus,
and `runs.jsonl` retains every observation. Final source/executable identities
and validation results are recorded in
`/private/tmp/otter-fill-length-{inputs,validation}.json`.

The full closing gate, full checked GC corpus and outstanding large-range
mutation/materialization policies remain open. No speed improvement is claimed;
the persistent Cargo cache and unrelated user files remain preserved.

## Complete and control every reverse pair

At `4024f6ef6`, `Array.prototype.reverse` independently truncates its loop to
half of `min(length, 2^25)`. A generic receiver of length `2^25 + 2` with a
throwing getter at index `2^24` returns normally instead of reaching that final
pair. The isolated pre-change CLI returns `[false,33554434]`; installed Node
returns `[true,33554434]` for the same exact thrown-identity probe. The
[standard reverse algorithm](https://tc39.es/ecma262/multipage/indexed-collections.html#sec-array.prototype.reverse)
visits all pairs up to `floor(captured length / 2)`. This is an observable
property-order defect, not a dense allocation limit.

The same native loop has no work or cancellation checkpoint. A 20,000-length
ordinary hole receiver completes with only 62 reported work units under a
1,000-unit rejecting budget. New public-runtime budget, yield and cancellation
tests all fail before the fix. The cancellation safety sentinel independently
bounds a broken implementation at a third native HasProperty callback; it
reports an invalid operand instead of the expected interruption.

The reverse driver now owns the full captured pair range and calls the
existing native iteration checkpoint before every pair. Rejection and
interruption retain fatal disposition; yielding rotates the owning work slice
without returning to the JavaScript job queue. Both-absent pairs still consume
work. There is no preliminary enumeration, eager scan, new allocation,
dynamic dispatch, or alternate receiver implementation. Existing ToObject and
length preparation, per-pair scoped roots and Has/Get/Set/Delete order remain
unchanged. Fill's separate materialization bound and admission are unchanged.

The runtime regression deliberately traverses the former ceiling rather than
using an early-throw large-length case as proof of complete range coverage.
Other cases cover ordinary and Array holes under a rejecting budget, fatal
completion bypassing source catches, runtime reuse after GC, exact native
HasProperty cancellation between pairs, and yielding without early microtask
delivery. The shared small corpus covers odd/even/short lengths, holes,
UTF-16 payload identity, a maximum-safe length with live length mutation,
ordered Proxy operations, exact thrown identity and no enumeration/prototype
prewalk. It passes on installed Node; the existing preparation corpus retains
all four reverse presence branches and moving-GC proofs.

The final affected runtime matrix passes **29/29 on ARM64** and **29/29 on
x86_64 under Rosetta**, including all five new tests and the complete range in
every execution policy. The standalone public CLI range probe now returns
`[true,33554434]` in interpreter, Template and production modes. All **1307 VM**
and **350 JIT unit tests** pass; the actual moving preparation/root-extent proof
also passes separately on x86_64. Existing native-generation, no-fallback,
real-OOM and GC assertions are unchanged. Strict workspace all-target/all-feature
Clippy, VM/runtime no-default-feature checks, formatting/diff checks and the
CLI/Test262/difftest build pass. The existing quick GC family passes at stride
1 and inherits the explicit persistent `CARGO_TARGET_DIR`.

Test262 reverse coverage remains **18/18**, with identical exact paths and
outcomes before/after and matching corpus, target, assertions and semantic
configuration. The standalone differential runner passes the new reverse,
preparation and fill-length-guard corpora across interpreter, Template,
production and GC stride 16, with zero timeouts. Pre-change CLI/runner binaries
and hashes remain in `/private/tmp/otter-reverse-control-before-tools`.

Reports are retained under `/private/tmp/otter-reverse-control-*`, including
`commands.json`, `tests-{before,after}.log`, `range-{before,after}.json`,
`reference.json`, `{host,x86}.log`, `units.log`, `clippy.stderr.log`,
`features.stderr.log`, `build.jsonl`, `rooting-x86.log`,
`test262-comparison.json`, `difftest.json`, `quick-gc.log` and `fmt.log`.

The new reverse corpus passes the existing process-isolated GC driver
**48/48** at strides 1–16 across all three execution policies.
`/private/tmp/otter-reverse-control-gc/results.json` reports `status: passed`
with zero failures, unchanged source/executable hashes and a complete
`runs.jsonl` journal. Final source/executable identities and results are
recorded in `/private/tmp/otter-reverse-control-{inputs,validation}.json`.

This closes reverse's range/control limitation. Fill and join materialization
bounds, the full closing gate and the full checked GC corpus remain separate
work. No speed improvement is claimed. The persistent Cargo target and
unrelated user files remain preserved; dependencies and toolchain are unchanged.
