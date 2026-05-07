# ES Conformance

This file tracks measured Test262 results for the active
`crates/otter-test262` runner.

## Runner Status

Captured on 2026-05-07 against engine commit
`92f417e7040408e72cf58d6d68b3c6addd8d38e7`.

The current runner CLI is:

```sh
cargo run -p otter-test262 --bin otter-test262 -- run <args>
```

Observed stale commands in repository docs/scripts:

- `--profile test262` is not defined in `Cargo.toml`.
- `--bin test262` does not exist; the bin target is `otter-test262`.
- The current runner has `run --filter ... --output ...`; older
  `--subdir`, `--save`, `--log`, and `-vv` flags are not accepted.
- `gen-conformance` and `merge-reports` bin targets are not present in
  `crates/otter-test262`.

Full corpus dry-run:

```sh
cargo run -p otter-test262 --bin otter-test262 -- run --dry-run
# total: 53179
```

No full Test262 run has been captured in this checkout yet.

## Targeted Baselines

### Object.hasOwn

Command:

```sh
cargo run -p otter-test262 --bin otter-test262 -- run \
  --filter built-ins/Object/hasOwn \
  --timeout 5000 \
  --output test262_results/current_object_hasown_before.json
```

Before:

| total | passed | failed | skipped | timeout | OOM | crash | pass rate |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 62 | 52 | 10 | 0 | 0 | 0 | 0 | 83.87% |

After installing JS-visible `Object` static methods and own-symbol
support for `Object.hasOwn`:

```sh
cargo run -p otter-test262 --bin otter-test262 -- run \
  --filter built-ins/Object/hasOwn \
  --timeout 5000 \
  --output test262_results/current_object_hasown_after.json
```

| total | passed | failed | skipped | timeout | OOM | crash | pass rate |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 62 | 54 | 8 | 0 | 0 | 0 | 0 | 87.10% |

Delta: +2 passing tests.

After installing VM-owned `Function.prototype.call` / `apply` /
`bind` / `toString` entries, JS-visible native-function `name` /
`length` metadata, and non-callback `Array.prototype` methods through
static bootstrap specs:

```sh
cargo run -p otter-test262 --bin otter-test262 -- run \
  --filter built-ins/Object/hasOwn \
  --timeout 5000 \
  --output test262_results/current_object_hasown_final.json
```

| total | passed | failed | skipped | timeout | OOM | crash | pass rate |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 62 | 55 | 7 | 0 | 0 | 0 | 0 | 88.71% |

Delta from first baseline: +3 passing tests.

### Function.prototype.call

Command:

```sh
cargo run -p otter-test262 --bin otter-test262 -- run \
  --filter built-ins/Function/prototype/call \
  --timeout 5000 \
  --output test262_results/current_function_prototype_call_after_arguments_object.json
```

Current:

| total | passed | failed | skipped | timeout | OOM | crash | pass rate |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 51 | 17 | 0 | 34 | 0 | 0 | 0 | 100.00% |

This subset now skips old sloppy-only/Sputnik `Function(...)` cases by
root config policy (`skip_flags = ["noStrict"]` plus targeted legacy
path ignores). All non-skipped tests in this focused subset now pass.

Remaining common blockers:

- `Object.hasOwn` still lacks full `ToPropertyKey` object coercion
  via `[Symbol.toPrimitive]`, `toString`, and `valueOf`.
- `arguments` now uses an unmapped descriptor-backed object for strict
  functions; sloppy mapped arguments remain intentionally out of scope
  while `noStrict` is skipped.

### ThrowTypeError

Command:

```sh
cargo run -p otter-test262 --bin otter-test262 -- run \
  --filter built-ins/ThrowTypeError \
  --timeout 5000 \
  --output test262_results/batch_throw_type_error_after_metadata.json
```

Current:

| total | passed | failed | skipped | timeout | OOM | crash | pass rate |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 14 | 13 | 0 | 1 | 0 | 0 | 0 | 100.00% |

Delta from the immediate batch baseline
`test262_results/batch_throw_type_error_before.json`: +4 passing tests.
The remaining skipped test is cross-realm coverage.

### Object.getOwnPropertyNames

Command:

```sh
cargo run -p otter-test262 --bin otter-test262 -- run \
  --filter built-ins/Object/getOwnPropertyNames \
  --timeout 5000 \
  --output test262_results/batch_object_gopn_after_primitives.json
```

Current:

| total | passed | failed | skipped | timeout | OOM | crash | pass rate |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 45 | 21 | 24 | 0 | 0 | 0 | 0 | 46.67% |

Delta from the immediate batch baseline
`test262_results/batch_object_gopn_after_native_function.json`: +4 passing
tests. The remaining failures cluster around Array instance/prototype
identity, richer object descriptor behavior, proxy invariants, and
additional ordinary object edge cases.
