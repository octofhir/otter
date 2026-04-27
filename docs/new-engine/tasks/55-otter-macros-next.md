# Task 55 — `otter-macros-next` proc-macro crate

## Goal

Replace the per-method boilerplate in
`crates-next/otter-vm/src/{string_prototype,array_prototype,
number/prototype,math/mod,regexp_prototype}.rs` with a focused
proc-macro crate that mirrors what legacy `crates/otter-macros` did
for the old runtime, but targets the new engine's much simpler ABI
(`fn(&IntrinsicArgs<'_>) -> Result<Value, IntrinsicError>`).

## Why

After tasks 30 + 31, every prototype file follows the same shape:

```rust
fn impl_<method>(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let recv = receiver_<kind>(args)?;
    let arg0 = arg_<type>(args, 0)?;
    let arg1 = arg_<type>_or(args, 1, default)?;
    // ...actual logic...
    Ok(Value::<kind>(out))
}
```

…with a trailing declarative `intrinsics!` registration. The
receiver guard, argument coercion, and error construction repeat
verbatim across 80+ functions. A proc-macro absorbs all of this
without hiding capability checks (we have none in the new engine)
or runtime sequencing.

## Scope

- New crate: `crates-next/otter-macros-next` (proc-macro = true).
- Macros to ship in v1:
  - `#[js_method]` — annotates a Rust function with optional `name`,
    `arity`, `receiver` attributes; the function signature describes
    the desired arg coercion (`fn(recv: &JsString, idx: i64,
    pad: Option<&JsString>) -> Result<JsString, IntrinsicError>`).
    The macro generates the wrapping `fn impl_*` matching the
    `IntrinsicFn` shape and registers it.
  - `js_proto!` — replaces `intrinsics!`, accepts a brace-delimited
    list of `#[js_method]`-annotated fns, emits the
    `IntrinsicTable` and `pub fn lookup`.
  - `#[js_namespace]` — covers Math / JSON / Reflect-style namespace
    objects (one table for properties, one for callable members).
- Migrate `string_prototype`, `array_prototype`, `number/prototype`,
  `math`, `regexp_prototype` to the new macros.
- Keep the old `intrinsics!` decl-macro as a thin compatibility shim
  during migration; delete once all five tables move.

## Out of scope

- GC integration, capability checks, async dispatch — these don't
  exist in the new engine and don't belong in the macro.
- Migrating `js_proto!` users into a single mega-file. Each
  prototype keeps its own module.

## Acceptance criteria

- All five prototype tables compile through the new macro.
- Engine fixture suite stays at 100 % pass.
- Each `impl_<method>` body is the actual JS-method logic, no
  receiver guard, no manual arg coercion.
- `#[js_method]` errors are reported at the annotation site (not
  the generated code) so editor diagnostics stay readable.

## Files / directories you may touch

- `crates-next/otter-macros-next/` (new crate).
- All five prototype files under `crates-next/otter-vm/src/`.
- Workspace `Cargo.toml`.

## Risks

- Proc-macro UX. If a `#[js_method]` signature can't be coerced
  cleanly, the error must point at the function (use `Span` from
  the input tokens, not the generated output). Test on every arg
  coercion shape we currently use.
- Compile-time cost. Keep the generated code shallow: prefer trait
  dispatch on the arg types over deeply-nested macro expansion.

## Status

- not started
