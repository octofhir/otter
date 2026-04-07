# otter-macros

Proc-macros for descriptor-driven JavaScript bindings in the active Otter runtime stack.

## What This Crate Is For

`otter-macros` exists to remove repetitive binding boilerplate without hiding runtime semantics.

Use these macros to describe:

- JS-visible classes
- JS-visible namespaces
- single native bindings
- grouped binding bundles
- host-owned object surfaces
- hosted native modules

Do not use macros here to hide:

- GC rooting and payload lifetime rules
- capability checks
- runtime scheduling boundaries
- tricky bootstrap ordering

## Macro Repertoire

The current active macro set is:

- `#[js_class]`
- `#[js_namespace]`
- `#[js_constructor]`
- `#[js_method]`
- `#[js_static]`
- `#[js_getter]`
- `#[js_setter]`
- `#[dive]`
- `raft!`
- `burrow!`
- `lodge!`

## Choosing The Right Macro

Use `#[js_class]` when:

- your JS API has a constructor
- you need `.prototype` methods
- you need static methods
- you need class getters/setters

Use `#[js_namespace]` when:

- the surface is a plain namespace object
- there is no constructor story
- the surface behaves like `Math`, `Reflect`, or a native namespace object

Use `#[dive]` when:

- you need one native function descriptor
- the main abstraction is one host callback
- you want descriptor metadata without hand-writing `NativeFunctionDescriptor`
- the binding is synchronous by default, or asynchronous via `deep`

Use `raft!` when:

- you already have several `#[dive]` functions
- they all target the same install target
- you want `Vec<NativeBindingDescriptor>`

Use `burrow!` when:

- you already have several `#[dive]` methods/getters/setters
- they belong on an existing native object handle
- you want `RuntimeState::install_burrow(...)` to install them

Use `lodge!` when:

- you are declaring a `HostedNativeModuleLoader`
- the module exports `#[dive]` functions and a few values
- you want module registration and export wiring in one place

Write manual code when:

- capability enforcement is the most important logic
- the installation order is delicate
- the macro would hide behavior more than it would clarify it

## Common Signature

The active runtime signature used by `#[dive]`, `#[js_class]`, and `#[js_namespace]` member methods is:

```rust
fn(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError>
```

For new runtime code, this is the only signature you should target.

## Setup

```toml
[dependencies]
otter-macros = { path = "../otter-macros" }
```

Typical imports:

```rust
use otter_macros::{
    burrow, dive, js_class, js_constructor, js_getter, js_method, js_namespace, js_setter,
    js_static, lodge, raft,
};
use otter_vm::{RegisterValue, RuntimeState, VmNativeCallError};
```

## `#[js_class]`

Use `#[js_class]` on:

- the struct declaration to declare the JS-visible class name
- the `impl` block to collect member metadata

Example:

```rust
use otter_macros::{js_class, js_constructor, js_getter, js_method, js_static};
use otter_vm::{RegisterValue, RuntimeState, VmNativeCallError};

#[js_class(name = "Counter")]
struct Counter;

#[js_class]
impl Counter {
    #[js_constructor(name = "Counter", length = 1)]
    fn constructor(
        this: &RegisterValue,
        _args: &[RegisterValue],
        _runtime: &mut RuntimeState,
    ) -> Result<RegisterValue, VmNativeCallError> {
        Ok(*this)
    }

    #[js_method(name = "inc", length = 1)]
    fn inc(
        this: &RegisterValue,
        args: &[RegisterValue],
        _runtime: &mut RuntimeState,
    ) -> Result<RegisterValue, VmNativeCallError> {
        let delta = args.first().and_then(|value| (*value).as_i32()).unwrap_or(1);
        this.add_i32(RegisterValue::from_i32(delta))
            .map_err(|error| VmNativeCallError::Internal(error.to_string().into()))
    }

    #[js_getter(name = "value")]
    fn value(
        this: &RegisterValue,
        _args: &[RegisterValue],
        _runtime: &mut RuntimeState,
    ) -> Result<RegisterValue, VmNativeCallError> {
        Ok(*this)
    }

    #[js_static(name = "zero", length = 0)]
    fn zero(
        _this: &RegisterValue,
        _args: &[RegisterValue],
        _runtime: &mut RuntimeState,
    ) -> Result<RegisterValue, VmNativeCallError> {
        Ok(RegisterValue::from_i32(0))
    }
}
```

What it generates:

- `Type::JS_CLASS_NAME`
- `Type::js_class_descriptor() -> JsClassDescriptor`
- one `*_descriptor()` helper per annotated member
- helper lists like `Type::js_methods()` and `Type::js_getters()`

When installing a class into the runtime, consume `Type::js_class_descriptor()` through the runtime/bootstrap layer. Do not rebuild the class shape manually if the descriptor path already exists.

### Field Attributes

On the struct form of `#[js_class]`, these marker attributes are also supported:

- `#[js_skip]` to exclude a field from generated property lists
- `#[js_readonly]` to classify a field as readonly metadata

Those attributes affect generated field-name helper lists only. They do not install runtime properties by themselves.

## `#[js_namespace]`

Use `#[js_namespace]` for namespace-style surfaces with methods and accessors but no constructor.

Example:

```rust
use otter_macros::{js_getter, js_method, js_namespace, js_setter};
use otter_vm::{RegisterValue, RuntimeState, VmNativeCallError};

#[js_namespace(name = "Tools")]
struct ToolsNamespace;

#[js_namespace]
impl ToolsNamespace {
    #[js_method(name = "double", length = 1)]
    fn double(
        _this: &RegisterValue,
        args: &[RegisterValue],
        _runtime: &mut RuntimeState,
    ) -> Result<RegisterValue, VmNativeCallError> {
        let value = args.first().and_then(|value| (*value).as_i32()).unwrap_or_default();
        Ok(RegisterValue::from_i32(value.saturating_mul(2)))
    }

    #[js_getter(name = "version")]
    fn version(
        _this: &RegisterValue,
        _args: &[RegisterValue],
        _runtime: &mut RuntimeState,
    ) -> Result<RegisterValue, VmNativeCallError> {
        Ok(RegisterValue::from_i32(1))
    }

    #[js_setter(name = "version")]
    fn set_version(
        _this: &RegisterValue,
        _args: &[RegisterValue],
        _runtime: &mut RuntimeState,
    ) -> Result<RegisterValue, VmNativeCallError> {
        Ok(RegisterValue::undefined())
    }
}
```

What it generates:

- `Type::JS_NAMESPACE_NAME`
- `Type::js_namespace_descriptor() -> JsNamespaceDescriptor`
- one `*_descriptor()` helper per annotated member
- helper lists like `Type::js_namespace_methods()`

Use this when the target installation path is a namespace/global/object builder flow, not a constructor-backed class flow.

## Member Marker Macros

These macros are used only inside `#[js_class]` or `#[js_namespace]` impl blocks:

- `#[js_constructor(name = "...", length = N)]`
- `#[js_method(name = "...", length = N)]`
- `#[js_static(name = "...", length = N)]`
- `#[js_getter(name = "...")]`
- `#[js_setter(name = "...")]`

These are marker macros. They do not install anything by themselves. The surrounding `#[js_class]` or `#[js_namespace]` macro reads them and emits descriptor metadata.

Rules:

- use exactly one member marker per annotated function
- use `js_static` only inside `#[js_class]`
- use getter/setter names that intentionally pair when you want one accessor property

## `#[dive]`

`#[dive]` is the otter-themed macro for one native binding.

`#[dive]` is synchronous by default.

`#[dive(deep)]` is the asynchronous variant of the same macro. It does not create a different binding model. It only switches the generated descriptor from sync method metadata to async method metadata.

For active-runtime functions, it generates:

- `FOO_NAME`
- `FOO_LENGTH`
- `foo_descriptor() -> NativeFunctionDescriptor`
- `foo_binding(target) -> NativeBindingDescriptor`

Example:

```rust
use otter_macros::dive;
use otter_vm::{RegisterValue, RuntimeState, VmNativeCallError};

#[dive(name = "now", length = 0)]
fn performance_now(
    _this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let millis = runtime.timers().now().as_secs_f64() * 1000.0;
    Ok(RegisterValue::from_number(millis))
}
```

Supported active flags:

- `name = "..."` for the JS-visible name
- `length = N` for `.length`
- `getter` for accessor getter metadata
- `setter` for accessor setter metadata
- `constructor` for constructor metadata
- `deep` to make `#[dive]` asynchronous

Descriptor behavior:

- `#[dive(...)]` generates sync method metadata
- `#[dive(deep, ...)]` generates async method metadata
- `getter`, `setter`, and `constructor` stay descriptor-shape flags on the same macro

Use `deep` only when the runtime path consuming the descriptor is prepared for async native entrypoints.

Example async variant:

```rust
#[dive(name = "readFile", deep, length = 1)]
fn read_file_async(
    _this: &RegisterValue,
    _args: &[RegisterValue],
    _runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    todo!()
}
```

Examples:

```rust
#[dive(name = "size", getter)]
fn size(...) -> Result<RegisterValue, VmNativeCallError> { ... }

#[dive(name = "value", setter)]
fn set_value(...) -> Result<RegisterValue, VmNativeCallError> { ... }

#[dive(name = "Thing", constructor, length = 1)]
fn construct_thing(...) -> Result<RegisterValue, VmNativeCallError> { ... }
```

## `raft!`

`raft!` groups several `#[dive]` bindings onto one install target and returns `Vec<NativeBindingDescriptor>`.

Example:

```rust
let bindings = raft! {
    target = Global,
    fns = [set_timeout, clear_timeout, queue_microtask]
};
```

Expansion model:

- each function must already be annotated with `#[dive]`
- the macro calls each generated `*_binding(NativeBindingTarget::...)`
- all bundled functions share the same target

Use `raft!` for:

- globals
- namespaces
- prototype or constructor bundles

Do not use `raft!` for host-owned object handles. That is what `burrow!` is for.

## `burrow!`

`burrow!` groups several `#[dive]` member descriptors for one host-owned object surface.

Example:

```rust
let members = burrow! {
    fns = [kv_set, kv_get, kv_size, kv_closed]
};
runtime.install_burrow(object, &members)?;
```

Expansion model:

- each function must already be annotated with `#[dive]`
- the macro calls each generated `*_descriptor()`
- it returns `Vec<NativeFunctionDescriptor>`
- `RuntimeState::install_burrow(...)` normalizes and installs the methods/accessors

Use `burrow!` for:

- native payload objects returned from modules
- host-owned objects without a constructor/global registration step
- object surfaces that would otherwise need repeated `install_method` / `install_getter`

## `lodge!`

`lodge!` generates a `HostedNativeModuleLoader` for the active runtime stack.

Example:

```rust
lodge!(
    kv_module,
    module_specifiers = ["otter:kv"],
    default = function(kv_open as "kv"),
    functions = [
        ("kv", kv_open),
        ("openKv", kv_open as "openKv"),
    ],
);
```

What it generates:

- a loader struct (`KvModule` for `kv_module`)
- `kv_module_entries() -> Vec<HostedExtensionModule>`
- descriptor-driven allocation for function exports
- namespace export wiring for functions and values

### `lodge!` Inputs

`name`

- snake_case module declaration name
- used to derive the generated loader type and `*_entries()` helper

`module_specifiers = ["..."]`

- one or more registered specifiers for the same loader

`default = object`

- creates a default export object
- mirrors `functions` and `values` onto both the namespace and the default object

`default = function(fn_name as "JsName")`

- installs a default export function
- does not mirror named exports onto that function object

`default = value(expr)`

- installs a default export value expression

`functions = [("exportName", rust_fn), ("otherName", rust_fn as "JsName")]`

- each `rust_fn` must already have `#[dive]`
- `exportName` is the module export key
- optional `as "JsName"` overrides the function object’s `.name`

`values = [("exportName", expr)]`

- `expr` must evaluate to `RegisterValue`
- the expression runs inside the generated `load(...)` function and may use `runtime`

### `lodge!` Example With Values

```rust
lodge!(
    ffi_module,
    module_specifiers = ["otter:ffi"],
    default = object,
    functions = [
        ("dlopen", ffi_dlopen),
        ("ptr", ffi_ptr),
    ],
    values = [
        (
            "suffix",
            RegisterValue::from_object_handle(runtime.alloc_string(platform_suffix()).0)
        ),
        (
            "FFIType",
            RegisterValue::from_object_handle(build_ffi_type_object(runtime)?.0)
        ),
    ],
);
```

Use `lodge!` when:

- you are defining `HostedNativeModuleLoader`
- the module exports are mostly functions and a few value objects
- you want one canonical declaration instead of a hand-written loader impl

## Naming Policy

Stable JS-shaped macros keep their `js_*` names:

- `js_class`
- `js_namespace`
- `js_constructor`
- `js_method`
- `js_static`
- `js_getter`
- `js_setter`

Otter-themed macros name the higher-level binding patterns:

- `dive` for one binding
- `raft` for one target bundle
- `burrow` for one host-owned object surface
- `lodge` for one hosted module loader

## Repository Usage Rules

For new code in the active runtime stack:

- prefer `#[js_class]` / `#[js_namespace]` for descriptor-visible JS surfaces
- prefer `#[dive]` for individual native bindings
- prefer `raft!` instead of hand-written `vec![*_binding(...)]`
- prefer `burrow!` instead of repeated `install_method` / `install_getter` on native objects
- prefer `lodge!` instead of hand-written `HostedNativeModuleLoader` impls

Avoid ad-hoc manual descriptor assembly when one of the macros already fits the shape.

## License

MIT
