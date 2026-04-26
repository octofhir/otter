# Task 26 — `class` declarations, `extends`, `super`

## Goal

Compile `class` declarations down to a constructor function with
methods on `prototype`, supporting single-class inheritance via
`extends` and `super` calls / lookups.

## Scope

- `class C { constructor(...) { ... } method() { ... } }` lowers
  to a constructor function whose prototype carries the methods.
- `class D extends C { ... }` chains prototypes, exposes `super.x`
  and `super(...)` calls.
- `static method() { ... }` puts the method on the constructor
  itself.
- Getters / setters in the class body land on the prototype as
  property descriptors (foundation: minimal accessor support
  sufficient for the syntax to work; full `[[Get]]`/`[[Set]]`
  semantics may stay limited).
- `new ClassName(args...)` (depends on a `new` opcode — introduce
  it here for both classes and plain functions).

## Out of scope

- Private fields (`#name`) — separate task.
- Decorators.
- Mixin / multiple inheritance helpers.

## Files / directories you may touch

- `crates-next/otter-compiler/` (class lowering).
- `crates-next/otter-bytecode/` (`Op::New`, accessor descriptors).
- `crates-next/otter-vm/`
- `tests/engine/classes/`

## Acceptance criteria

- Basic `class Animal { constructor(n) { this.name = n; } speak()
  { return this.name + " speaks"; } }; new Animal("dog").speak()`
  returns `"dog speaks"`.
- `extends` + `super(...)` works for fields and methods.
- Static method dispatch works.
- Engine suite green.

## Verification commands

```bash
cargo run -p otter-cli -- test --suite engine --filter classes/
```

## Risks

- Constructor / `super` ordering rules in derived classes — read
  the spec carefully and add explicit fixtures for the common
  mistakes.

## Status

- not started
