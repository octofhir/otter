# Task 22 — Closures with upvalues

## Goal

Inner functions can read and (where allowed by `let`/`const`)
mutate variables declared in an enclosing scope.

## Scope

- Compiler tracks which outer-scope variables a nested function
  references; each becomes an "upvalue".
- A closure value holds:
  - the function id;
  - a `SmallVec<[UpvalueRef; 4]>` of captured slots.
- Upvalue cells are heap-shared: when an outer-scope `let`
  declaration is captured, mutation through any closure (or the
  outer scope) sees the latest value.
- New opcodes:
  - `MakeClosure <dst> <function_id_const> <upvalue_count> <upvalues...>`.
  - `LoadUpvalue <dst> <slot>`.
  - `StoreUpvalue <src> <slot>`.
- `MakeFunction` keeps working for closure-less functions; the
  compiler picks `MakeFunction` when the function has zero
  upvalues, `MakeClosure` otherwise.

## Out of scope

- `with` statement.
- `eval`-induced dynamic captures.

## Files / directories you may touch

- `crates-next/otter-bytecode/`
- `crates-next/otter-vm/` (closure / upvalue module).
- `crates-next/otter-compiler/` (capture analysis pass).
- `tests/engine/calls/closures/`

## Acceptance criteria

- Counter pattern works:
  ```ts
  function makeCounter() {
      let n = 0;
      return () => { n = n + 1; return n; };
  }
  let c = makeCounter();
  c(); c(); c(); // 3
  ```
- Mutual recursion through nested locals works.
- Captured `const` cannot be assigned (compile-time error).
- Engine suite green.

## Verification commands

```bash
cargo run -p otter-cli -- test --suite engine --filter calls/closures/
```

## Risks

- Lifetime of upvalue cells: foundation can use `Rc<RefCell<Value>>`
  for clarity. Real GC integration arrives later.
- Capture analysis must not over-capture (only variables actually
  referenced from the inner function).

## Status

- not started
