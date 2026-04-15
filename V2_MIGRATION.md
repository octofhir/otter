# V2 Migration Tracker

Canonicalizing OtterJS onto a single v2 bytecode pipeline. One milestone per commit; the quality gate runs green before each.

Design doc: internal — see repo memory / conversation history. The prior scoping doc `V2_MIGRATION_PLAN.md` was the pre-approval draft and is retained for history only.

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
| M1       | `function f(n) { return n + 1 }` end-to-end: Identifier, int32 NumericLiteral, `+` with `AddSmi`/`Add`.                         | [ ]    |        |
| M2       | JIT stencil disassembly sanity + M1 microbenchmark.                                                                             | [ ]    |        |
| M3       | Remaining int32 binary ops: `-`, `*`, `|`, `&`, `^`, `<<`, `>>`, `>>>`.                                                         | [ ]    |        |
| M4       | Local `let`/`const` with initializer.                                                                                          | [ ]    |        |
| M5       | `AssignmentExpression` (`=`, `+=`, `-=`, `*=`, `|=`) onto a local `let`.                                                        | [ ]    |        |
| M6       | `IfStatement` + relational ops (`<`, `>`, `<=`, `>=`, `===`, `!==`) for int32.                                                  | [ ]    |        |
| M7       | `WhileStatement`. Closes `bench2.ts`: int32 accumulator loop + full microbench vs bun/node.                                     | [ ]    |        |
| M8       | `ForStatement` (desugar to while).                                                                                             | [ ]    |        |
| M9       | Multiple functions + `CallExpression` without `this`/closures.                                                                  | [ ]    |        |
| M_JIT_x86_64 | Cranelift / hand-rolled x86_64 backend for the JIT baseline.                                                               | [ ]    |        |
| M10+     | Closures, globals, `console.log`, classes, async, generators, destructuring, property access, exceptions, exports/imports.       | [ ]    |        |

## AST coverage (v2 source compiler)

| Construct                         | Supported | Milestone |
|-----------------------------------|-----------|-----------|
| `Program`                         | no        | —         |
| `FunctionDeclaration`             | no        | M1        |
| `Identifier`                      | no        | M1        |
| `NumericLiteral` (int32-safe)     | no        | M1        |
| `BinaryExpression` `+` int32      | no        | M1        |
| `BinaryExpression` other arith    | no        | M3        |
| `VariableDeclaration` `let/const` | no        | M4        |
| `AssignmentExpression`            | no        | M5        |
| `IfStatement`                     | no        | M6        |
| `WhileStatement`                  | no        | M7        |
| `ForStatement`                    | no        | M8        |
| `CallExpression`                  | no        | M9        |

## Benchmarks

Empty until M7 lands. After M7 the `bench2.ts` sum-loop is the baseline for latency vs `bun run` and `node --experimental-vm-modules`, recorded for aarch64 interpreter and aarch64 JIT.

| Scenario                          | Otter interp | Otter JIT | bun | node |
|-----------------------------------|--------------|-----------|-----|------|
| `bench2.ts` (10⁶ iter)            | —            | —         | —   | —    |

## Notes

- v1 still reachable via `git show <pre-M0-commit>:<path>` for historical reference; the working tree contains only v2.
- Until M1 lands, `otter run foo.js` returns a `SourceLoweringError::Unsupported { construct: "program", ... }` and a non-zero exit status. This is the expected post-M0 state.
- Integration tests deleted in M0 are rebuilt incrementally — each subsequent milestone adds fresh tests for the feature it introduces.
