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

## Large FindViaPredicate ranges: separate confirmed defect

Read-only follow-up probes found that the callback driver's pathological-length
approximation also changes `find*` semantics. This remains outside the committed
Get/HasProperty correction and has not been fixed or added to the running gate.

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
These are public CLI reproductions, not new passing regressions.

The intended owner remains the existing callback driver: lazy ordered ranges
can preserve early termination without allocating an index vector or walking
prototypes. Before removing the approximation, verify interruption and work
budget enforcement for repeated callbacks, including native and compiled
predicates. A huge range must remain cancellable; silently skipping indices is
not a substitute. The other callback methods' sparse approximation needs its own
scope and validation rather than an incidental broad rewrite.

## Confirmed next tasks

1. **P0: fix premature string coercion in optimized Add.** Use the preserved
   reduced reproduction and native artifacts; require current operand proof
   before conversion and retain native admission, effect order and moving roots.
2. **P1: diagnose the GC-stress watchdog failures.** Keep exact failed paths and
   limits. Re-run the full closing gate after correcting the causes, then the
   separate full checked GC corpus at strides 1–16. Do not turn timeouts into
   passing agreement or hide them by filtering the corpus.
3. **P1: isolate baseline startup-image SIGSEGV.** Compare raw/restored launches
   under the same release profile before attributing the failure to image or
   debug-information settings. Current shipped-image conformance is not inferred
   from the raw baseline comparison.
4. **P1: correct large FindViaPredicate ranges.** Retain exact forward/reverse
   indexed Get order and early termination, with interruption/budget proof and
   public regressions. Preserve the already-fixed Get-without-HasProperty rule.
5. **P1: repeat build measurements on an idle host.** At least three alternating
   before/after clean-build pairs, a separate linking measurement, and a restorative
   build between incremental triggers. Do not promise a 50% LOC reduction without
   identifying the responsibilities actually removed and the checks preserved.
