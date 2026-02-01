# Otter: Rust Intrinsics — Status & Remaining Work

Status: **builtins.js REMOVED** — all core ECMAScript intrinsics are Rust-native.

## Architecture

All builtins live in `crates/otter-vm-core/src/intrinsics.rs` + `intrinsics_impl/` modules:

| Module | Lines | Scope |
|--------|-------|-------|
| `helpers.rs` | ~50 | Utility functions (strict_equal, same_value_zero) |
| `array.rs` | ~1300 | Array.prototype (all methods incl. callbacks + iterators) |
| `boolean.rs` | ~140 | Boolean constructor + prototype |
| `date.rs` | ~970 | ALL Date.prototype methods (ES2026) |
| `function.rs` | ~260 | Function.prototype (call, apply, bind, toString) |
| `map_set.rs` | ~1650 | Map/Set/WeakMap/WeakSet + ES2025 set algebra + iterators |
| `math.rs` | ~470 | Math (8 constants + 37 methods, ES2026) |
| `number.rs` | ~420 | Number.prototype methods |
| `promise.rs` | ~550 | Promise constructor + statics + prototype |
| `reflect.rs` | ~430 | Reflect (all 13 ES2015+ methods) |
| `regexp.rs` | ~700 | RegExp constructor + all prototype methods |
| `string.rs` | ~990 | String.prototype methods + Symbol.iterator |
| `temporal.rs` | ~150 | Temporal namespace |
| `generator.rs` | ~280 | Generator.prototype + AsyncGenerator.prototype (ES2026) |
| `typed_array.rs` | ~850 | TypedArray.prototype + all 11 typed array types (ES2026) |

Intrinsics initialization: 4 stages in `intrinsics.rs`:
1. `allocate()` — pre-allocate all prototype objects + well-known symbols
2. `wire_prototype_chains()` — set up [[Prototype]] links
3. `init_core()` — initialize all methods with correct property attributes
4. `install_on_global()` — install constructors on global object

Callback dispatch uses `InterceptionSignal` enum: native function detects closure callback, returns interception signal, interpreter catches it and calls `call_function()` in a loop with full VM context.

## Remaining Work

### Completed Recently

✅ **TypedArray implementation** (2026-02-01) — Complete ES2026-compliant TypedArray implementation with all 11 types (Int8Array through BigUint64Array). Includes:
  - Full intrinsics system integration with %TypedArray%.prototype and 11 specific prototypes
  - All constructors supporting 5 forms: empty, length, typedArray, buffer view, arrayLike
  - Static methods: BYTES_PER_ELEMENT, from(), of()
  - All prototype methods: at, copyWithin, fill, includes, indexOf, join, lastIndexOf, reverse, set, slice, subarray, toString, toLocaleString
  - Getters: buffer, byteLength, byteOffset, length
  - Iterators: values, keys, entries, Symbol.iterator
  - Proper object-based storage with hidden __TypedArrayData__ property for getter/method access

✅ **Generator/AsyncGenerator prototypes** (2026-02-01) — `Generator.prototype` and `AsyncGenerator.prototype` fully implemented as proper intrinsics with ES2026-compliant prototype chains, all methods (next, return, throw), Symbol.iterator/asyncIterator, and Symbol.toStringTag.

✅ **Iterator protocol completeness** (2026-02-01) — Map/Set/String iterators fully implemented with `%IteratorPrototype%` chain, Symbol.iterator support, UTF-16 surrogate pair handling for strings, and snapshot semantics for stable iteration.

### Medium Priority

1. **Proxy handler traps** — Proxy constructor exists but not all handler traps are fully validated.

### Low Priority

1. **Extract `intrinsics_impl/object.rs`** — Object.prototype and Object static methods currently inline in `intrinsics.rs`, could be extracted for maintainability.

2. **Extract `intrinsics_impl/error.rs`** — Error hierarchy init is inline in `intrinsics.rs`.

## Task 2: Runtime Job/Microtask Drain Correctness

Once builtins are stable, tighten microtask drain semantics:

- Drain microtasks/jobs after synchronous script execution, module evaluation, and Promise resolution.
- Define a single "drain jobs" loop called at well-defined points.
- Location: `crates/otter-vm-runtime/src/otter_runtime.rs`

## Appendix: Test262 Runner Improvements

Delegated to another agent. Key gaps in `crates/otter-test262`:

- `flags: [async]` handling — intercept `print()` for `"Test262:AsyncTestComplete"` (Boa style)
- `flags: [module]` handling — run as module, not script
- Negative expectations — honor `negative.phase` and validate error type
- `$262` host object — `createRealm`, `evalScript`, `detachArrayBuffer`, `gc`
