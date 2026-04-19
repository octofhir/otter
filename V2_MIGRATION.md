# V2 Migration Tracker

Canonicalizing OtterJS onto a single v2 bytecode pipeline. One milestone per commit; the quality gate runs green before each.

Design doc: this file — it's both the tracker and the forward plan. Commit messages, `JIT_REFACTOR_PLAN.md`, and repo memory carry the fine-grained history behind each milestone.

## Quality gate (all four green before a milestone commits)

```
timeout 180 cargo build --workspace
timeout 90  cargo clippy --workspace --all-targets -- -D warnings
timeout 30  cargo fmt --all --check
timeout 180 cargo test --workspace
```

Cross-target sanity (`cargo build --target`) is run for `aarch64-apple-darwin`, `x86_64-apple-darwin`, `x86_64-unknown-linux-gnu`, `x86_64-pc-windows-msvc` when the toolchain is available.

## Milestones

| ID       | Scope                                                                                                                          | Status | Commit |
|----------|--------------------------------------------------------------------------------------------------------------------------------|--------|--------|
| M0       | Retire v1 pipeline, delete legacy tests, canonicalize v2 naming, scaffold empty `ModuleCompiler`, wire CLI.                    | [x]    | eeb84c8 |
| M1       | `function f(n) { return n + 1 }` end-to-end: Identifier, int32 NumericLiteral, `+` with `AddSmi`/`Add`.                         | [x]    | 377ddd2 |
| M2       | JIT stencil disassembly sanity + M1 microbenchmark.                                                                             | [x]    | c11e064 |
| M3       | Remaining int32 binary ops: `-`, `*`, `|`, `&`, `^`, `<<`, `>>`, `>>>`.                                                         | [x]    | 62c2760 |
| M4       | Local `let`/`const` with initializer.                                                                                          | [x]    | 0a8cc3f |
| M5       | `AssignmentExpression` (`=`, `+=`, `-=`, `*=`, `|=`) onto a local `let`.                                                        | [x]    | 53c24a2 |
| M6       | `IfStatement` + relational ops (`<`, `>`, `<=`, `>=`, `===`, `!==`) for int32.                                                  | [x]    | 991b282 |
| M7       | `WhileStatement`. Closes `bench2.ts`: int32 accumulator loop + full microbench vs bun/node.                                     | [x]    | d02fce5 |
| M8       | `ForStatement` (desugar to while).                                                                                             | [x]    | 5ad7cfe |
| M9       | Multiple functions + `CallExpression` without `this`/closures.                                                                  | [x]    | f6ea6a5 |

### JIT track (ships first, blocks M10+)

See [`JIT_REFACTOR_PLAN.md`](./JIT_REFACTOR_PLAN.md) for concrete task lists per milestone.

| ID        | Scope                                                                                                                          | Status | Commit |
|-----------|--------------------------------------------------------------------------------------------------------------------------------|--------|--------|
| M_JIT_A   | Finish aarch64 tag-guarded v2 baseline: `eor/tst/b.ne` on every int32 load, bailout prologue, invocation through `TierUpHook::execute_cached`, widen analyzer coverage. | [x]    | 96d8534   |
| M_JIT_B   | x86_64 baseline backend — port the v2 template-baseline stencil (same op coverage, tag guards, bailout model).                   | [x]    | e1b907a   |
| M_JIT_C.1 | Mid-loop OSR — per-loop-header trampolines + `TierUpHook::execute_cached_at_pc` + back-edge budget-driven entry.                 | [x]    | 5251b41   |
| M_JIT_C.2 | Speculative int32-trust elision — feedback-driven tag-guard skipping on stable arithmetic PCs.                                   | [x]    | ad3d137   |
| M_JIT_C.3 | Loop-local register allocator — pin hot int32 slots into callee-saved registers across the loop body.                            | [x]    | 5fe7c1e   |

### Feature track (after JIT completion)

Each row is one shippable slice, committed as a `feat(vm): … (Mxx)` pair plus a `docs(v2-migration): record Mxx commit hash …` follow-up.

| ID       | Scope                                                                                                                          | Status | Commit |
|----------|--------------------------------------------------------------------------------------------------------------------------------|--------|--------|
| M10      | `UnaryExpression` (`!`, `-x`, `+x`, `typeof`, `void`, `~x`) + `UpdateExpression` (`++x`, `x++`, `--x`, `x--`) on locals.         | [x]    | 4cea559 |
| M11      | `break` / `continue` inside `while` / `for` (unlabelled).                                                                      | [x]    | 2bdb704 |
| M12      | Block scoping for `let` / `const` inside `if` / `while` / `for` bodies + nested blocks.                                         | [x]    | f0df9b2 |
| M13      | `ConditionalExpression` (`a ? b : c`) + logical `&&` / `\|\|` / `??` short-circuit.                                              | [x]    | 54e1339 |
| M14      | Global reads — `undefined`, `null`, `Infinity`, `NaN`, `globalThis`, plus one anchor builtin namespace.                         | [x]    | 8ca80bd |
| M15      | `StringLiteral` + string concatenation (`+` on mixed operands).                                                                 | [x]    | bca0df8 |
| M16      | `ObjectExpression` + `ArrayExpression` literals with int/string values.                                                         | [x]    | 13dd2f8 |
| M17      | Property access: `StaticMemberExpression` (`o.x`), `ComputedMemberExpression` (`o[k]`), read + write.                            | [x]    | 8f1c068 |
| M18      | Template literals (simple + interpolated).                                                                                       | [x]    | f8ddb59 |
| M19      | `console.log` + minimal console shim — first "hello world" gate.                                                                | [x]    | 4a41715 |
| M20      | `SwitchStatement` with `case` / `default` + `break` exits.                                                                     | [x]    | 5bd4a2d |
| M21      | `throw` + `try` / `catch` / `finally`.                                                                                         | [x]    | fe76e4b |
| M22      | Default params (`function f(n = 0)`) + rest params (`...rest`).                                                                 | [x]    | 03cb16f |
| M23      | Spread in call args + array literals (`f(...a)`, `[...a, ...b]`).                                                              | [x]    | b5cd2f4 |
| M24      | Destructuring patterns (array + object) in `let` bindings and params.                                                           | [x]    | a474ae9 |
| M25      | Closures — nested `FunctionDeclaration` / `FunctionExpression` + upvalue capture.                                                | [x]    | f0a39a0 |
| M26      | Arrow functions + lexical `this` binding.                                                                                       | [x]    | 6ecd547 |
| M27      | Class declaration: constructor + instance methods + static methods.                                                             | [x]    | 0911c9d |
| M28      | Class inheritance (`extends` + `super` + `super(args)` in constructor).                                                         | [x]    | 0981ec9 |
| M29      | Class private fields (`#x`) + accessor methods (`get` / `set`).                                                                 | [x]    | c9398fe |
| M30      | `for (x of arr)` + iterator protocol (`Symbol.iterator`, `next()`).                                                             | [x]    | cf63969 |
| M31      | `for (k in obj)` + property iteration.                                                                                         | [x]    | f38cddd |
| M32      | Promise runtime + microtask queue.                                                                                             | [x]    | b123787 |
| M33      | `async` functions + `await` expression.                                                                                         | [ ]    |        |
| M34      | Generators (`function*`, `yield`, `yield*`).                                                                                   | [ ]    |        |
| M35      | ES module imports + exports.                                                                                                    | [ ]    |        |
| M36      | `BigIntLiteral` + BigInt arithmetic + `RegExpLiteral` + basic `RegExp` match.                                                  | [ ]    |        |

Ordering follows a dependency chain where possible (`console.log` after property access + strings, `extends` after class declaration), but nothing is chiselled — M15/M16 can swap, M24/M25 can swap, etc. Re-order at execution time as real blocking dependencies surface.

## AST coverage (v2 source compiler)

| Construct                         | Supported | Milestone |
|-----------------------------------|-----------|-----------|
| `Program` (single top-level fn)   | yes       | M1        |
| `FunctionDeclaration`             | yes       | M1        |
| `Identifier` (parameter ref)      | yes       | M1        |
| `NumericLiteral` (int32-safe)     | yes       | M1        |
| `BinaryExpression` `+` int32      | yes       | M1        |
| `BinaryExpression` `-`/`*`/`\|`/`&`/`^`/`<<`/`>>`/`>>>` int32 | yes | M3        |
| `VariableDeclaration` `let/const` | yes       | M4        |
| `AssignmentExpression` (`=`/`+=`/`-=`/`*=`/`\|=`) | yes | M5 |
| `IfStatement`                     | yes       | M6        |
| `BinaryExpression` `<`/`>`/`<=`/`>=`/`===`/`!==` int32 | yes | M6 |
| `WhileStatement`                  | yes       | M7        |
| `VariableDeclaration` multi-declarator | yes  | M7        |
| `ForStatement`                    | yes       | M8        |
| Multiple top-level functions      | yes       | M9        |
| `CallExpression` (known function) | yes       | M9        |
| `UnaryExpression` (`!`, `-x`, `+x`, `~x`, `typeof`, `void`) | yes | M10 |
| `UpdateExpression` (`++x`, `x++`, `--x`, `x--`) on locals | yes | M10 |
| `BreakStatement` / `ContinueStatement` (unlabelled) | yes | M11 |
| Block scoping for `let` / `const` in `{ }` / `if` / `while` / `for` bodies | yes | M12 |
| `ConditionalExpression` (`a ? b : c`) | yes | M13 |
| `LogicalExpression` (`&&`, `\|\|`, `??`) | yes | M13 |
| `NullLiteral` / `BooleanLiteral` | yes | M14 |
| Well-known globals: `undefined`, `NaN`, `Infinity`, `globalThis`, `Math` | yes | M14 |
| `StringLiteral` + `+` string concatenation (mixed operands) | yes | M15 |
| `ObjectExpression` / `ArrayExpression` literals with int/string/bool/null values | yes | M16 |
| `StaticMemberExpression` / `ComputedMemberExpression` read + write (incl. compound `<op>=`) | yes | M17 |
| `TemplateLiteral` (simple + interpolated; nested; escape cooked-values) | yes | M18 |
| Method-call `CallExpression` (`o.m()`, `o[k]()`) + `console` global + host-function dispatch | yes | M19 |
| `SwitchStatement` with `case` / `default` + `break` exits (fall-through, mid-switch default, nested, string/content compare) | yes | M20 |
| `ThrowStatement` + `TryStatement` with `catch` / `finally` (nested, rethrow, bindingless catch; normal + exception paths only) | yes | M21 |
| Multi-param signatures + default initializers + rest params + writable param bindings | yes | M22 |
| Spread in `ArrayExpression` and method-call args (`[...a, ...b]`, `o.m(...xs)`); direct-call spread deferred | yes | M23 |
| Destructuring in `let` + params: array (with rest), object (shorthand, renaming, defaults); no nested/holes/object-rest | yes | M24 |
| `FunctionExpression` + nested `FunctionDeclaration` as first-class values + live upvalue capture (outer params/locals, chained grand-closures) | yes | M25 |
| `ArrowFunctionExpression` (concise / block body, captures outer bindings, curried chains); async arrows deferred | yes | M26 |
| `ClassDeclaration` (nested) + `NewExpression` + `ThisExpression`: constructor, instance + static methods, §9.2.2.1 return override; extends/fields/accessors/computed keys deferred | yes | M27 |
| Class `extends` heritage + `super.x` / `super[k]` / `super.m(args)` / `super(args)` in derived constructors; default derived ctor synthesis `constructor(...args) { super(...args); }`; home-object wiring for methods + constructor | yes | M28 |
| Class public + private instance fields (`x = …`, `#x = …`), static fields, `get` / `set` accessor methods (instance + static), `this.#x` / `obj.#x` read+write + compound `<op>=`, `#name in obj`; private methods/accessors deferred | yes | M29 |
| Private methods + private get/set accessors (`#m()`, `get #p()`, `set #p(v)`; instance + static), `obj.#m(args)` invocation, and `static { … }` blocks evaluated at class-definition time with `this = class` | yes | M29.5 |
| `for (<let\|const\|ident> of <iterable>) body` over built-in iterables (Array/String/TypedArray) via `GetIterator` + new `IteratorStep` opcode; `break` / `continue` wired through the loop-label stack; `for await`, destructuring LHS, `var` LHS, custom Symbol.iterator iterables deferred | yes | M30 |
| `Symbol` global + spec-compliant §7.4 iterator protocol: `GetIterator` does `@@iterator` lookup + call, `IteratorStep` falls back to user `.next()` call with `{value, done}` unpack, `ToPropertyKey` recognises symbol keys (`obj[Symbol.iterator] = …`) | yes | M30-tail |
| `for (<let\|const\|ident> in <source>) body` via `ForInEnumerate` + `ForInNext`, `null` / `undefined` source skips without throwing; class-method installation switched to `DefineClassMethod` so prototype methods stay non-enumerable per §15.7.11 | yes | M31 |
| `Promise` global + `new Promise(executor)` / `Promise.resolve` / `Promise.reject` / `.then` / `.catch` / `.finally` chaining from user source; `execute_with_runtime` drains the microtask queue before returning so promise callbacks settle before the host regains control | yes | M32 |

## Benchmarks

After M7 the `bench2.ts` sum-loop becomes the baseline for latency vs `bun run` and `node --experimental-vm-modules`, recorded first for aarch64 interpreter/JIT and then for the x86_64 baseline backend once `M_JIT_B` lands. Until then the M2 `f(42)` micro-row tracks per-call interpreter latency for the M1 lowering — useful as a regression floor while later milestones widen the source-compiler subset.

Reproduce the M2 + M7 rows with:

```bash
cargo test -p otter-jit --release -- --ignored m1_microbench --nocapture
cargo test -p otter-jit --release -- --ignored bench2_microbench --nocapture
```

| Scenario                          | Otter interp | Otter JIT | bun | node |
|-----------------------------------|--------------|-----------|-----|------|
| `f(42)` (10⁶ iter, aarch64)       | 496 ns/iter  | —         | —   | —    |
| `bench2.ts sum(10⁶)` per-call (50× warmup-100, aarch64, feedback-warm recompile) | 447 ms/call | 1.08 ms/call | — | — |
| `bench2.ts sum(10⁶)` per-inner-iter (aarch64, feedback-warm recompile) | 447 ns/iter | 1 ns/iter | — | — |
| `bench2.ts sum(10⁶)` per-call (1× warmup-1, x86_64-apple-darwin under Rosetta) | 1348 ms/call | 3.30 ms/call | — | — |
| `bench2.ts sum(10⁶)` per-inner-iter (x86_64-apple-darwin under Rosetta) | 1348 ns/iter | 3 ns/iter | — | — |

## Notes

- v1 still reachable via `git show <pre-M0-commit>:<path>` for historical reference; the working tree contains only v2.
- Integration tests deleted in M0 are rebuilt incrementally — each milestone adds fresh tests for the feature it introduces.
- The x86_64 `bench2_microbench` rows above were measured with `OTTER_BENCH2_CALLS=1 OTTER_BENCH2_WARMUP_CALLS=1` on Apple Silicon because the default 50-call release benchmark exceeds the fixed 180-second timeout under Rosetta; the benchmark's default path is unchanged for native x86_64 hosts.
