# E1 environment contract (decision record)

Implements section 5 of `2026-09-27-architecture.md`. Inventories and the
adversarial review that produced these decisions are in ignored
`benchmarks/results/arch-2026-09-27/e1/` (compiler, vm-runtime,
eval-with-dynamic, template-jit, machine-jit, tests-fixtures, critic).

Landing order: **E1a** replaces per-binding cells with per-scope contexts
(closures stay old-space objects); **E1b** makes closures young and movable
and allocates contexts/closures inline in generated code. Each lands as one
signed commit; there is no intermediate mode in which both models exist.

## Runtime objects

- `ContextBody` (new GC type, tag 0x21, internal value family):
  `{ scope_function_id: u32, scope_index: u16, slot_count: u16,
     parent: Value (context or undefined), extension: Value (eval extension or
     undefined), slots: [Value; slot_count] }` — 24 B + 8 B per slot plus the
  GC header. Parent and extension are full 8-byte `Value` words so generated
  code dereferences them without cage-base arithmetic. TDZ = `Value::hole()`.
  Allocated young (old under `tenure_all`), payload fully initialized before
  any safepoint, every slot/extension store write-barriered.
- Scope identity is `(function_id, scope_index)` into the owning CodeBlock's
  descriptor table; a live context keeps its function id alive in
  `code_liveness`.
- `EvalExtensionBody` replaces `EvalEnvBody` (tag 0x2E kept): names + values of
  bindings a sloppy direct eval created at run time; no parent, no cells, no
  sequence numbers. Heap-image capture asserts none exists.
- Contexts are an internal `Value` family: never `is_object_type`, never
  `typeof`, never type feedback.
- `JsClosureBody` loses the capture tail, `upvalue_base`, `upvalue_count`,
  `eval_env` and `bound_derived_this`; `ClosureCallHeader` gains
  `context: Value`. Arrow `bound_this` / `bound_new_target` remain value copies
  (immutable per activation). In E1a closures stay old-space objects.
- Frames (`Frame`, `NativeFrame`, `ParkedFrameState`) hold no spine and no
  eval environment. SELF is a required frame input (no default) and is always
  the exact closure; `LoadClosureContext` reads SELF.

## Scope descriptors (bytecode `Function.scopes`)

`ScopeDescriptor { kind, flags { strict, var_scope, has_extension }, slots:
Vec<SlotDescriptor { name: String (owned), kind, flags { exported } }> }`.

Kinds: `FunctionName` (self binding of a named function expression) ←
`Callee` (extension only for sloppy + parameter expressions + eval in the
parameters) ← `Params` ← `Body` (var scope; extension for a sloppy function
whose own code calls eval) ← `Lexical`; plus `Block`, `Catch`, `ForHead`,
`Switch`, `With`, `Class`, `ObjectHome`, `EvalVar`, `EvalLexical`, `Module`.
Each context is allocated only when its scope owns at least one slot or an
extension anchor.

Slot kinds decide initial value and checks: `Var`, `FunctionDecl`,
`Arguments`, simple `Param` → `undefined`, never hole; `Let`, `Const`,
`Class`, `DerivedThis`, `Param` with parameter expressions and no duplicates
→ hole (checked reads). `FnSelfName`, `CatchParam{simple}`, `WithObject`,
`PrivateName`, `PrivateBrand`, `SuperHome`, `SuperStaticHome`, `SuperCtor`,
`ClassSelf`, `ModuleEnv`, `ImportMeta`, `Synthetic` complete the alphabet.

A binding is a context slot iff it is captured by a nested function, visible
to any direct eval (strict or sloppy) in its own or a nested scope, a mapped
formal, the derived-constructor `this` captured by an arrow/eval, or a module
binding reached from nested code. Otherwise it is a register.

## Bytecode operations

Coordinates are packed `coord = depth << 16 | slot` (slot < 65536, else a
compile error). `ctx` operands are registers holding a context (or
`undefined` for "no context").

| Op | Operands | Semantics |
| --- | --- | --- |
| `LoadClosureContext` | W | context of SELF, `undefined` if none |
| `LoadSelf` | W | SELF (replaces the self-name MakeFunction trick) |
| `CreateContext` | W, R parent, IMM scope | allocate; init slots from descriptor |
| `CopyContext` | W, R src | per-iteration copy (§14.7.4.4); same scope/parent |
| `LoadContextSlot` | W, R ctx, IMM coord | read; no hole check |
| `LoadContextSlotChecked` | W, R ctx, IMM coord | hole → ReferenceError (name from descriptor; DerivedThis message) |
| `StoreContextSlot` | R, R ctx, IMM coord | initializing store + barrier |
| `StoreContextSlotChecked` | R, R ctx, IMM coord | hole → ReferenceError, else store |
| `BindThisContextSlot` | R, R ctx, IMM coord | non-hole → ReferenceError (super twice), else store |
| `ReturnDerived` | R value, R this_raw | object → value; undefined → this (hole → ReferenceError); else TypeError |
| `MakeClosure` | W, CONST fn, R ctx | closure over `ctx`; no capture operands |
| `LoadLookupSlot` | W, R ctx, CONST name, IMM coord | probe extensions of extension-capable contexts at hops `[0, depth)`, else checked slot read |
| `StoreLookupSlot` | R, R ctx, CONST name, IMM coord, IMM policy | extension hit writes; miss applies fallback policy to the slot |
| `DeleteLookupSlot` | W, R ctx, CONST name, IMM depth | extension hit removes → true, miss → false |
| `LoadLookupGlobal` / `TypeofLookupGlobal` | W, R ctx, CONST name, IMM depth | extensions at hops `[0, depth)`, then global (throw / undefined) |
| `StoreLookupGlobal` | R, R ctx, CONST name, IMM depth\|strict<<31 | extensions, then global store |
| `DeleteLookupGlobal` | W, R ctx, CONST name, IMM depth | extensions, then global delete |
| `ResolveLookupRef` | W ref, R ctx, CONST name, IMM coord or GLOBAL | §13.15.2 base before RHS: extension, target context, or `undefined` (global) |
| `StoreRef` | R, R ref, CONST name, IMM slot\|policy\|strict | store through the pre-resolved base |
| `DeclareEvalVar` | R ctx, CONST name, IMM var_depth | create-if-absent in the var-scope extension |
| `StoreVarScope` | R, R ctx, CONST name, IMM var_depth | set-or-create in the var-scope extension |
| `Eval` | W, R src, R ctx, IMM flags | innermost context replaces the site index |

Deleted: `LoadUpvalue`, `StoreUpvalue`, `StoreUpvalueChecked`, `FreshUpvalue`,
`LoadShadowedUpvalue`(`Snap`), `StoreShadowedUpvalueChecked`(`Snap`),
`DeleteShadowedUpvalue`, `EvalBindingSeq`, `EvalRestoreBinding`,
`LoadDynamic`, `StoreDynamic`, `TypeofDynamic`, `DeleteDynamic`,
`Function.{own_upvalue_count, inherited_upvalue_count, direct_eval_bindings,
eval_sites}`, `DirectEvalBinding`, `ArgumentBindingStorage::Upvalue`
(→ `Context { reg, slot }`).

Implicit register reads are forbidden: derived-constructor completion uses
`ReturnDerived`; `CallForwardArguments` names the mapped-arguments context
register explicitly.

## Decision record

| # | Question | Decision |
| --- | --- | --- |
| C1 | Arrow `this`/`new.target` | value copies on the arrow closure; only derived-constructor `this` is a slot |
| C2 | Eval shadowing | `Lookup*` + `ResolveLookupRef`/`StoreRef`; the isolate sequence counter and snapshots are deleted |
| C3 | Eval `<main>` incoming context | SELF closure over the caller context |
| C4 | Field widths | 8-byte `Value` words for parent, extension, closure context |
| C5 | Coordinates | packed `depth<<16 \| slot`, u16 slot |
| C6 | Stubs | one stub set; `MakeClosure` is a non-reentrant allocation |
| C7 | Derived bind | distinct `BindThisContextSlot` |
| C8 | Binding schema | only checked/lookup/dynamic ops split blocks |
| C9 | SELF shortcut in MakeClosure | deleted; `LoadSelf` |
| C10 | Extension anchor | the FunctionName ← Callee ← Params ← Body ← Lexical chain |
| C12 | Descriptor names | owned strings (contexts outlive and cross chunks) |
| C13 | Eval-visible bindings | any direct eval, strict or sloppy |
| — | `ForEachAddBitAndUpvalue` array fast path | deleted (benchmark-shaped matcher) |

Pre-existing defects found by the review and owned by this work: derived-class
instance fields run only after a statement-level `super()` (five shapes differ
from Node); try/finally bodies skip BlockDeclarationInstantiation; ESM export
mirroring decided by name. They get difftest programs and fixes in this series.

## Validation

difftest (interpreter oracle, Template, production, stress 1/4/16 with slot
verification), otter-jit lib, GC stress 1..16 in all tiers on the closure,
eval, arguments, class, loop and generator corpora, Test262 `language/` and
`annexB/language/` (baseline: 24,077/24,077 passing, 0 failing), all seven
fixed-work workloads with the allocation census.
