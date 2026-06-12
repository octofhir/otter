# Error Model — Research + Unified Design

Goal: one coherent, **structured** error model. Stop stringifying/reformatting at
every boundary; carry `kind` + `code` + the thrown `Value` as data. Driven by Node
conformance needing `error.code` (`ERR_INVALID_ARG_TYPE`, …) — but the wins are
engine-wide.

## Current landscape (4 representations + 1 side channel)

1. **`ErrorKind`** (`otter-vm/src/error_classes.rs`) — JS class: `Error`,
   `TypeError`, `RangeError`, `SyntaxError`, `ReferenceError`, `URIError`, … The
   canonical "kind". GOOD.
2. **`VmError`** (`otter-vm/src/run_control.rs`) — VM execution result. Mixes:
   - host/structural: `MissingReturn`, `InvalidOperand`, `OutOfMemory`,
     `Interrupted`, `StackOverflow`, `Exit`, `BudgetExceeded`.
   - user JS errors: `TypeError{message}`, `RangeError{message}`,
     `SyntaxError{message}`, `URIError{message}`, `TypeMismatch*`,
     `UndefinedIdentifier{name}`, … — **one variant per kind, message-only**.
   - `Uncaught { value: String }` — **stringifies the thrown Error object** (lossy:
     loses identity / `.code` / `instanceof`).
   - `JsonError { code, message }` — the ONLY code-carrying variant (ad-hoc proof
     the pattern is needed).
3. **`NativeError`** (`otter-vm/src/native_function.rs`) — native-boundary result.
   `Thrown{name,message}`, `TypeError/RangeError/SyntaxError/URIError/
   ReferenceError{name, reason}`, `Exit{code}`, `OutOfMemory`. Every variant
   carries a redundant `name: &'static str` (the *native fn* name) used only to
   `format!("{name}: {reason}")` — noise Node never emits. No `code`, no `Value`.
4. **`OtterError`** (`otter-runtime/src/error.rs`) — embedder result. JS-error case
   = `Runtime { Box<Diagnostic> }`; plus `Compile`, `Io`, `Config`, `Timeout`,
   `OutOfMemory`, `Capability`, `Interrupted`, `Internal`.
5. **`Diagnostic`** (`otter-runtime/src/diagnostics/mod.rs`) — RICH struct: `kind`,
   `code` (wire string), `message`, `source_url`, `range`, `span`, `help`,
   `frames`, `cause`, `aggregated_errors`. This is the good target shape.
6. **`pending_uncaught_throw: Option<Value>`** (`Interpreter`) — the ACTUAL thrown
   JS Error object. The real source of truth, carried in a SIDE CHANNEL because
   `VmError::Uncaught` can't hold a `Value` (see below).

## Problems (the "string ебуиз")
- **P1 — Uncaught is lossy.** The thrown Error `Value` is the source of truth but
  `VmError::Uncaught{String}` stringifies it; the real `Value` is smuggled through
  `pending_uncaught_throw`. Two representations, one lossy. `native_to_vm_error`
  does `Thrown{message} → Uncaught{value: message}` (string).
- **P2 — boundary reformatting.** `native_to_vm_error`: `format!("{name}: {reason}")`
  munges the native-fn name into the message. `vm_error_to_throwable` then treats
  it as the message → user sees `path.basename: ...` noise.
- **P3 — no structured `code`.** Node `ERR_*` codes have nowhere to live
  (only `JsonError` hacks it in). `error.code` cannot be produced by native code.
- **P4 — redundant per-kind variants + redundant `name`.** Four near-identical
  `{message}` variants in VmError and five `{name,reason}` in NativeError.
- **P5 — three enums, lossy conversions.** `vm_to_native_error` /
  `native_to_vm_error` / `map_vm_error` / `map_native_error` each drop or reshape
  structure.

## Why `Uncaught` is a `String` today
`VmError: std::error::Error + Display`, and `Display` has **no heap access** — it
can't render a `Value`. That forced the stringify. Fix: keep a cheap rendered
string for `Display` *and* carry the `Value` for identity. The `Value` is the
truth; the string is only a fallback label.

## Target design

### 1. One structured JS-error payload
Replace the per-kind VmError/NativeError variants with a single struct:
```rust
pub struct JsErrorData {
    pub kind: ErrorKind,            // TypeError, RangeError, ...
    pub code: Option<&'static str>, // Node "ERR_*"; None for plain JS errors
    pub message: String,
}
```
- `VmError::Js(JsErrorData)` replaces `TypeError/RangeError/SyntaxError/URIError`
  (keep `TypeMismatch*`/`NotCallable`/… as conveniences that lower to `Js`).
- `NativeError::Js(JsErrorData)` replaces `TypeError/RangeError/.../{name,reason}`.
  Drop the redundant `name` (no more `"{name}: {reason}"`).

### 2. Thrown values stay values
- `VmError::Uncaught { value: Value, rendered: String }` — `value` is the real
  Error object (identity/`.code`/`instanceof` preserved); `rendered` backs
  `Display` only. Collapses the `pending_uncaught_throw` duplication (pending
  becomes the carrier feeding `value`).
- `NativeError::Thrown { value: Value }` — re-throw a real value verbatim (Proxy
  traps, `require` propagation) with zero stringification.

### 3. `.code` attaches at materialization
`vm_error_to_throwable_with_stack_roots` (`error_ops.rs:165`) is the single place a
`VmError` becomes an Error `Value`. For `VmError::Js(data)` with `data.code`, set an
own non-enumerable `code` property on the instance (mirrors how `message` is set).
Node `error.code` then "just works".

### 4. Embedder boundary reads structure, not strings
`enrich_runtime_diagnostic_with_cause` already takes the pending `Value`. Read the
Error object's `code`/`name`/`message` into the `Diagnostic` fields (`Diagnostic`
already has `code`). One structured hop, no re-parse.

### 5. Native ergonomics (no strings at call sites)
Curated `NativeCtx`/runtime helpers:
```rust
ctx.throw_type_error(msg)                  -> NativeError::Js{TypeError, None, msg}
ctx.throw_coded(ErrorKind::TypeError, "ERR_INVALID_ARG_TYPE", msg)
ctx.throw_value(error_value)               -> NativeError::Thrown{value}
```
Node modules throw `ERR_INVALID_ARG_TYPE` structurally; the code flows to the
instance and to `Diagnostic`.

### 6. assert can finally inspect
`assert.throws(fn, matcher)`: `take_pending_uncaught_throw()` → the Error `Value`
→ read `.code`/`.name`/`.message` via a pub `[[Get]]` helper → validate matcher.
No strings.

## Migration (incremental, each step green)
1. Add `JsErrorData` + `VmError::Js` + `NativeError::Js` ALONGSIDE existing
   variants; route the per-kind variants through `Js` internally. Keep `Display`.
2. `Uncaught { value, rendered }`; thread the real `Value` (fold in
   `pending_uncaught_throw`). Fix `Display` to use `rendered`.
3. Attach `.code` in `vm_error_to_throwable`; read it into `Diagnostic` at the
   embedder boundary.
4. Add native throw helpers; migrate `path` (and future modules) to
   `ERR_INVALID_ARG_TYPE`.
5. `assert.throws`/`rejects` matcher validation via the thrown `Value` + pub
   `[[Get]]`.
6. Delete the redundant per-kind variants + the `name` munging once callers move.

Net: `kind`+`code`+`Value` carried as data end-to-end; strings only at the final
human render. Unblocks Node `ERR_*` conformance across every module.
