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
| M_JIT_C.3 | Loop-local register allocator — pin hot int32 slots into callee-saved registers across the loop body.                            | [x]    | _pending_ |

### Feature track (after JIT completion)

Each row is one shippable slice, committed as a `feat(vm): … (Mxx)` pair plus a `docs(v2-migration): record Mxx commit hash …` follow-up.

| ID       | Scope                                                                                                                          | Status | Commit |
|----------|--------------------------------------------------------------------------------------------------------------------------------|--------|--------|
| M10      | `UnaryExpression` (`!`, `-x`, `+x`, `typeof`, `void`) + `UpdateExpression` (`++x`, `x++`, `--x`, `x--`) on locals.              | [ ]    |        |
| M11      | `break` / `continue` inside `while` / `for` (unlabelled).                                                                      | [ ]    |        |
| M12      | Block scoping for `let` / `const` inside `if` / `while` / `for` bodies + nested blocks.                                         | [ ]    |        |
| M13      | `ConditionalExpression` (`a ? b : c`) + logical `&&` / `\|\|` / `??` short-circuit.                                              | [ ]    |        |
| M14      | Global reads — `undefined`, `null`, `Infinity`, `NaN`, `globalThis`, plus one anchor builtin namespace.                         | [ ]    |        |
| M15      | `StringLiteral` + string concatenation (`+` on mixed operands).                                                                 | [ ]    |        |
| M16      | `ObjectExpression` + `ArrayExpression` literals with int/string values.                                                         | [ ]    |        |
| M17      | Property access: `StaticMemberExpression` (`o.x`), `ComputedMemberExpression` (`o[k]`), read + write.                            | [ ]    |        |
| M18      | Template literals (simple + interpolated).                                                                                       | [ ]    |        |
| M19      | `console.log` + minimal console shim — first "hello world" gate.                                                                | [ ]    |        |
| M20      | `SwitchStatement` with `case` / `default` + `break` exits.                                                                     | [ ]    |        |
| M21      | `throw` + `try` / `catch` / `finally`.                                                                                         | [ ]    |        |
| M22      | Default params (`function f(n = 0)`) + rest params (`...rest`).                                                                 | [ ]    |        |
| M23      | Spread in call args + array literals (`f(...a)`, `[...a, ...b]`).                                                              | [ ]    |        |
| M24      | Destructuring patterns (array + object) in `let` bindings and params.                                                           | [ ]    |        |
| M25      | Closures — nested `FunctionDeclaration` / `FunctionExpression` + upvalue capture.                                                | [ ]    |        |
| M26      | Arrow functions + lexical `this` binding.                                                                                       | [ ]    |        |
| M27      | Class declaration: constructor + instance methods + static methods.                                                             | [ ]    |        |
| M28      | Class inheritance (`extends` + `super` + `super(args)` in constructor).                                                         | [ ]    |        |
| M29      | Class private fields (`#x`) + accessor methods (`get` / `set`).                                                                 | [ ]    |        |
| M30      | `for (x of arr)` + iterator protocol (`Symbol.iterator`, `next()`).                                                             | [ ]    |        |
| M31      | `for (k in obj)` + property iteration.                                                                                         | [ ]    |        |
| M32      | Promise runtime + microtask queue.                                                                                             | [ ]    |        |
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
- Until M1 lands, `otter run foo.js` returns a `SourceLoweringError::Unsupported { construct: "program", ... }` and a non-zero exit status. This is the expected post-M0 state.
- Integration tests deleted in M0 are rebuilt incrementally — each subsequent milestone adds fresh tests for the feature it introduces.
- The x86_64 `bench2_microbench` rows above were measured with `OTTER_BENCH2_CALLS=1 OTTER_BENCH2_WARMUP_CALLS=1` on Apple Silicon because the default 50-call release benchmark exceeds the fixed 180-second timeout under Rosetta; the benchmark's default path is unchanged for native x86_64 hosts.
