//! Zero-cost JavaScript / module surface macros for Otter.
//!
//! Otter intrinsics, classes, and hosted modules are declared with a
//! family of otter-themed macros. Each macro corresponds to one role
//! in the JS / module surface; expansion produces ordinary Rust code
//! plus a `BuiltinIntrinsic`-shaped installer that bootstrap walks
//! at startup. No new runtime path, no dynamic registration — the
//! macros are pure code generation over the spec types in
//! [`otter_vm`] and the native ABI v1 documented at
//! [`docs/native-call-abi.md`](../../../docs/native-call-abi.md).
//!
//! # Naming theme
//!
//! Otters live in **holts** (single-otter dens), gather on land in
//! **couches**, float together in **rafts**, dig **burrows** for
//! private stashes, raise families in **lodges**, **dive** to forage,
//! grow a **pelt** for protection, and **groom** their fur to keep it
//! waterproof. Each term names exactly one macro role:
//!
//! | Role                                           | Macro       | Mnemonic                                |
//! |------------------------------------------------|-------------|-----------------------------------------|
//! | Namespace intrinsic (non-constructible)        | [`holt!`]   | a den that holds methods + constants    |
//! | Class intrinsic (callable ctor + proto)        | [`couch!`]  | a couch of otters — ctor + instances    |
//! | Grouped method spec (table form)               | [`raft!`]   | a raft of methods floating together     |
//! | Single binding (annotates one Rust fn)         | `#[dive]`   | one focused act                         |
//! | Host-owned object surface                      | [`burrow!`] | a private stash the embedder owns       |
//! | Hosted module loader (`otter:fs`, `node:url`)  | [`lodge!`]  | the family residence — module home      |
//! | `SafeTraceable` derive (GC body fields)        | `#[derive(Pelt)]` | the coat that keeps roots alive   |
//! | `Finalize` derive (drop-time cleanup)          | `#[derive(Groom)]` | the cleanup ritual              |
//!
//! The theme is in the macro identifiers only. Generated diagnostics
//! stay neutral / spec-leaning — no "your raft is sinking" error
//! messages.
//!
//! # Examples
//!
//! Namespace intrinsic — `Math` in the abstract:
//!
//! ```rust,ignore
//! use otter_macros::holt;
//! use otter_vm::{NativeCtx, NativeError, Value};
//!
//! holt! {
//!     name = "Math",
//!     feature = CORE,
//!     constants = [
//!         ("PI", f64, std::f64::consts::PI, read_only),
//!         ("E",  f64, std::f64::consts::E,  read_only),
//!     ],
//!     methods = raft! {
//!         "abs"  / 1 => native_abs,
//!         "ceil" / 1 => native_ceil,
//!         "pow"  / 2 => native_pow,
//!     },
//! }
//!
//! fn native_abs(_ctx: &mut NativeCtx<'_>, args: &[Value])
//!     -> Result<Value, NativeError> { /* … */ Ok(Value::undefined()) }
//! # fn native_ceil(_: &mut NativeCtx<'_>, _: &[Value]) -> Result<Value, NativeError> { Ok(Value::undefined()) }
//! # fn native_pow(_: &mut NativeCtx<'_>, _: &[Value]) -> Result<Value, NativeError> { Ok(Value::undefined()) }
//! ```
//!
//! Class intrinsic — `Proxy` in the abstract:
//!
//! ```rust,ignore
//! use otter_macros::{couch, raft};
//!
//! couch! {
//!     name = "Proxy",
//!     feature = CORE,
//!     constructor = (length = 2, call = proxy_ctor_call),
//!     statics = raft! {
//!         "revocable" / 2 => proxy_revocable_call,
//!     },
//! }
//! ```
//!
//! Single-method binding — fold one Rust function into the enclosing
//! `holt!` / `couch!` without listing it in the `raft!` table:
//!
//! ```rust,ignore
//! #[dive(name = "fromEpochMilliseconds", length = 1)]
//! pub fn from_epoch_ms(ctx: &mut NativeCtx<'_>, args: &[Value])
//!     -> Result<Value, NativeError> { /* … */ }
//! ```
//!
//! Hosted module — `otter:kv` in the abstract:
//!
//! ```rust,ignore
//! use otter_macros::lodge;
//!
//! lodge! {
//!     prefix = "otter",
//!     name   = "kv",
//!     capabilities = [Net("kv.example.com")],
//!     exports = raft! {
//!         "get"  / 1 => kv_get,
//!         "set"  / 2 => kv_set,
//!         "open" / 1 => kv_open,
//!     },
//! }
//! ```
//!
//! GC body derives — `Pelt` for tracing, `Groom` for finalize:
//!
//! ```rust,ignore
//! use otter_macros::{Pelt, Groom};
//!
//! #[derive(Pelt, Groom)]
//! struct MyBody {
//!     target:  otter_gc::Gc<otter_vm::JsObject>,
//!     #[pelt(skip)] // not a GC slot
//!     cached_hash: u64,
//! }
//! ```
//!
//! # Invariants
//!
//! - Exported JavaScript names and arity are explicit in macro
//!   metadata; the macro never infers them from Rust identifiers.
//! - Expansion emits `NamespaceSpec`, `ClassSpec`, `ConstructorSpec`,
//!   and `MethodSpec` static data with `NativeCall::Static` function
//!   pointers — same shape as the hand-written installers in
//!   [`otter_vm::intrinsics`](../../../crates/otter-vm/src/intrinsics/).
//! - Bootstrap remains explicit; generated specs are installed by
//!   JS surface builders or the centralized bootstrap registry.
//! - Generated code compiles under `#![forbid(unsafe_code)]`; any
//!   macro that needs `unsafe` for its expansion is a design bug.
//!
//! # See also
//!
//! - [Design note](../../../docs/otter-macros-design.md) — full
//!   surface, naming rationale, migration sequence.
//! - [Refactor tracker](../../../docs/otter-macros-refactor-tracker.md)
//!   — per-consumer port state.
//! - [Native call ABI](../../../docs/native-call-abi.md) — the
//!   signature every generated method targets.
//! - [Macro overview (mdbook)](../../../docs/book/src/macros/overview.md)
//!   — narrative chapter with per-macro examples.
//!
//! # Status
//!
//! Phase 4.1 of the architecture refactor — see
//! `docs/architecture-refactor-plan-2026-05.md` Task 4.1. The legacy
//! [`js_namespace`] / [`js_class`] / `js_fn` / `js_constructor`
//! attribute macros are kept temporarily for backward compatibility
//! during the cutover; they are deleted in sub-phase 4.1b once the
//! otter-themed surface is fully populated. Do not use them in new
//! code.

use std::collections::{BTreeMap, BTreeSet};

use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::parse::{Parse, ParseStream};
use syn::{
    Attribute, Ident, Item, ItemMod, LitInt, LitStr, Meta, Path, Result, Token, Visibility, braced,
    bracketed, parenthesized, parse_macro_input,
};

mod couch;
mod holt;

/// Generate a `NamespaceSpec` + `BuiltinIntrinsic` adapter for a
/// non-constructible namespace intrinsic (`Math`, `JSON`, `Reflect`,
/// `Atomics`, `Console`, `Symbol` namespace surface, `Temporal`
/// top-level, `Intl`).
///
/// See the crate-level docs for the naming theme and full surface,
/// and [`docs/otter-macros-design.md`](../../../docs/otter-macros-design.md)
/// for the design rationale.
///
/// # Syntax
///
/// ```rust,ignore
/// holt! {
///     name = "Math",
///     feature = CORE,
///     methods = {
///         "abs"  / 1 => native_abs,
///         "ceil" / 1 => native_ceil,
///         "pow"  / 2 => native_pow,
///     },
/// }
/// ```
///
/// `feature` is the bare variant name from
/// `::otter_vm::BootstrapFeatures`. Optional fields `spec =
/// MATH_SPEC,` and `intrinsic = MathIntrinsic,` override the
/// derived ident names (default: `<NAME>_SPEC` and `Intrinsic`).
///
/// # Generated symbols
///
/// - `pub static <SPEC>: ::otter_vm::NamespaceSpec`
/// - `pub struct <INTRINSIC>;`
/// - `impl ::otter_vm::BuiltinIntrinsic for <INTRINSIC>` with
///   `NAME`, `FEATURE`, and `install`.
///
/// Bootstrap registration stays explicit — add
/// `crate::bootstrap_entry!(<INTRINSIC>)` to `BOOTSTRAP_ENTRIES`
/// in `crates/otter-vm/src/bootstrap.rs`.
#[proc_macro]
pub fn holt(input: TokenStream) -> TokenStream {
    holt::expand(input)
}

/// Generate a `ConstructorSpec` + `BuiltinIntrinsic` adapter for a
/// class intrinsic — callable constructor with its own static
/// methods plus a prototype slot.
///
/// Used for `Proxy`, `Date`, `Map`, `Set`, `Promise`, `RegExp`, the
/// Temporal classes, every error class, every TypedArray.
///
/// # Syntax
///
/// ```rust,ignore
/// couch! {
///     name = "Proxy",
///     feature = CORE,
///     constructor = (length = 2, call = proxy_ctor_call),
///     statics = {
///         "revocable" / 2 => proxy_revocable_call,
///     },
/// }
/// ```
///
/// Optional fields `spec = MY_SPEC,` and `intrinsic = MyIntrinsic,`
/// override the derived ident names. Prototype methods + accessors
/// are not yet supported by this skeleton — see the design note
/// for the planned grammar.
///
/// # Generated symbols
///
/// - `pub static <SPEC>: ::otter_vm::ConstructorSpec`
/// - `pub struct <INTRINSIC>;`
/// - `impl ::otter_vm::intrinsic_install::BuiltinIntrinsic for
///   <INTRINSIC>` whose `install` body allocates the
///   `NativeFunction` constructor and pins each static as an own
///   data property before binding the constructor on `globalThis`.
#[proc_macro]
pub fn couch(input: TokenStream) -> TokenStream {
    couch::expand(input)
}

/// Generate a static `NamespaceSpec` from a Rust module.
///
/// The module must declare an explicit JavaScript name and spec
/// identifier:
///
/// ```rust,ignore
/// #[otter_macros::js_namespace(name = "Math", spec = MATH_SPEC)]
/// mod math {
///     #[js_fn(name = "abs", length = 1)]
///     pub fn abs(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
///         todo!()
///     }
/// }
/// ```
///
/// Expansion keeps the module intact, removes the consumed `js_fn`
/// attributes, and emits a static spec equivalent to handwritten
/// surface data.
#[proc_macro_attribute]
pub fn js_namespace(attr: TokenStream, item: TokenStream) -> TokenStream {
    let args = parse_macro_input!(attr as NamespaceArgs);
    let mut module = parse_macro_input!(item as ItemMod);

    let Some((_, items)) = module.content.as_mut() else {
        return syn::Error::new_spanned(
            &module.ident,
            "js_namespace requires an inline module body",
        )
        .to_compile_error()
        .into();
    };

    let mut methods = Vec::new();
    let mut seen = BTreeSet::new();
    for item in items.iter_mut() {
        let Item::Fn(func) = item else {
            continue;
        };
        match take_js_fn(&mut func.attrs) {
            Ok(Some(spec)) => {
                if !seen.insert(spec.name.value()) {
                    return syn::Error::new_spanned(
                        &func.sig.ident,
                        format!("duplicate js_fn name `{}`", spec.name.value()),
                    )
                    .to_compile_error()
                    .into();
                }
                methods.push(MethodBinding {
                    rust_name: func.sig.ident.clone(),
                    js_name: spec.name,
                    length: spec.length,
                });
            }
            Ok(None) => {}
            Err(err) => return err.to_compile_error().into(),
        }
    }

    let spec_ident = args.spec;
    let namespace_name = args.name;
    let methods_ident = format_ident!("__OTTER_{}_METHODS", spec_ident);
    let mod_ident = module.ident.clone();
    let method_entries = methods.iter().map(|method| {
        let rust_name = &method.rust_name;
        let js_name = &method.js_name;
        let length = &method.length;
        quote! {
            ::otter_vm::MethodSpec {
                name: #js_name,
                length: #length,
                attrs: ::otter_vm::Attr::builtin_function(),
                call: ::otter_vm::NativeCall::Static(#mod_ident::#rust_name),
            }
        }
    });

    quote! {
        #module

        #[allow(non_upper_case_globals)]
        static #methods_ident: &[::otter_vm::MethodSpec] = &[
            #(#method_entries),*
        ];

        #[allow(non_upper_case_globals)]
        #[doc = "Generated static JavaScript namespace spec."]
        pub static #spec_ident: ::otter_vm::NamespaceSpec = ::otter_vm::NamespaceSpec {
            name: #namespace_name,
            methods: #methods_ident,
            accessors: &[],
            constants: &[],
            attrs: ::otter_vm::Attr::global_binding(),
        };
    }
    .into()
}

/// Generate a static `ClassSpec` from a Rust module.
///
/// The module must contain exactly one `#[js_constructor(length = N)]`
/// function. Prototype instance methods use `#[js_method(...)]`;
/// constructor/static-side methods use `#[js_static_method(...)]`.
/// Prototype accessors use `#[js_getter(...)]` and
/// `#[js_setter(...)]`.
///
/// ```rust,ignore
/// #[otter_macros::js_class(name = "Point", spec = POINT_SPEC)]
/// mod point {
///     #[js_constructor(length = 1)]
///     pub fn construct(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
///         todo!()
///     }
///
///     #[js_method(name = "valueOf", length = 0)]
///     pub fn value_of(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
///         todo!()
///     }
/// }
/// ```
///
/// Expansion keeps the module intact, removes consumed helper
/// attributes, and emits a static class spec over the JS surface
/// builder backend.
#[proc_macro_attribute]
pub fn js_class(attr: TokenStream, item: TokenStream) -> TokenStream {
    let args = parse_macro_input!(attr as ClassArgs);
    let mut module = parse_macro_input!(item as ItemMod);

    let Some((_, items)) = module.content.as_mut() else {
        return syn::Error::new_spanned(&module.ident, "js_class requires an inline module body")
            .to_compile_error()
            .into();
    };

    let mut constructor = None;
    let mut static_methods = Vec::new();
    let mut prototype_methods = Vec::new();
    let mut prototype_accessors = BTreeMap::<String, AccessorBinding>::new();
    let mut seen_static = BTreeSet::new();
    let mut seen_prototype = BTreeSet::new();

    for item in items.iter_mut() {
        let Item::Fn(func) = item else {
            continue;
        };

        let ctor = match take_length_attr(&mut func.attrs, "js_constructor") {
            Ok(value) => value,
            Err(err) => return err.to_compile_error().into(),
        };
        let static_fn = match take_fn_attr(&mut func.attrs, "js_static_method") {
            Ok(value) => value,
            Err(err) => return err.to_compile_error().into(),
        };
        let proto_fn = match take_fn_attr(&mut func.attrs, "js_method") {
            Ok(value) => value,
            Err(err) => return err.to_compile_error().into(),
        };
        let getter = match take_name_attr(&mut func.attrs, "js_getter") {
            Ok(value) => value,
            Err(err) => return err.to_compile_error().into(),
        };
        let setter = match take_name_attr(&mut func.attrs, "js_setter") {
            Ok(value) => value,
            Err(err) => return err.to_compile_error().into(),
        };

        let helper_count = usize::from(ctor.is_some())
            + usize::from(static_fn.is_some())
            + usize::from(proto_fn.is_some())
            + usize::from(getter.is_some())
            + usize::from(setter.is_some());
        if helper_count > 1 {
            return syn::Error::new_spanned(
                &func.sig.ident,
                "only one JS class helper attribute is allowed per function",
            )
            .to_compile_error()
            .into();
        }

        if let Some(ctor) = ctor {
            if constructor.is_some() {
                return syn::Error::new_spanned(
                    &func.sig.ident,
                    "duplicate js_constructor in one js_class module",
                )
                .to_compile_error()
                .into();
            }
            constructor = Some(ConstructorBinding {
                rust_name: func.sig.ident.clone(),
                length: ctor.length,
            });
        }

        if let Some(spec) = static_fn {
            if !seen_static.insert(spec.name.value()) {
                return syn::Error::new_spanned(
                    &func.sig.ident,
                    format!("duplicate js_static_method name `{}`", spec.name.value()),
                )
                .to_compile_error()
                .into();
            }
            static_methods.push(MethodBinding {
                rust_name: func.sig.ident.clone(),
                js_name: spec.name,
                length: spec.length,
            });
        }

        if let Some(spec) = proto_fn {
            if prototype_accessors.contains_key(&spec.name.value()) {
                return syn::Error::new_spanned(
                    &func.sig.ident,
                    format!("js_method conflicts with accessor `{}`", spec.name.value()),
                )
                .to_compile_error()
                .into();
            }
            if !seen_prototype.insert(spec.name.value()) {
                return syn::Error::new_spanned(
                    &func.sig.ident,
                    format!("duplicate js_method name `{}`", spec.name.value()),
                )
                .to_compile_error()
                .into();
            }
            prototype_methods.push(MethodBinding {
                rust_name: func.sig.ident.clone(),
                js_name: spec.name,
                length: spec.length,
            });
        }

        if let Some(spec) = getter {
            if seen_prototype.contains(&spec.name.value()) {
                return syn::Error::new_spanned(
                    &func.sig.ident,
                    format!("js_getter conflicts with js_method `{}`", spec.name.value()),
                )
                .to_compile_error()
                .into();
            }
            let key = spec.name.value();
            let accessor = prototype_accessors
                .entry(key)
                .or_insert_with(|| AccessorBinding::new(spec.name));
            if accessor.get.is_some() {
                return syn::Error::new_spanned(
                    &func.sig.ident,
                    format!("duplicate js_getter name `{}`", accessor.js_name.value()),
                )
                .to_compile_error()
                .into();
            }
            accessor.get = Some(func.sig.ident.clone());
        }

        if let Some(spec) = setter {
            if seen_prototype.contains(&spec.name.value()) {
                return syn::Error::new_spanned(
                    &func.sig.ident,
                    format!("js_setter conflicts with js_method `{}`", spec.name.value()),
                )
                .to_compile_error()
                .into();
            }
            let key = spec.name.value();
            let accessor = prototype_accessors
                .entry(key)
                .or_insert_with(|| AccessorBinding::new(spec.name));
            if accessor.set.is_some() {
                return syn::Error::new_spanned(
                    &func.sig.ident,
                    format!("duplicate js_setter name `{}`", accessor.js_name.value()),
                )
                .to_compile_error()
                .into();
            }
            accessor.set = Some(func.sig.ident.clone());
        }
    }

    let Some(constructor) = constructor else {
        return syn::Error::new_spanned(
            &module.ident,
            "js_class requires one #[js_constructor(length = N)] function",
        )
        .to_compile_error()
        .into();
    };

    let spec_ident = args.spec;
    let class_name = args.name;
    let mod_ident = module.ident.clone();
    let static_methods_ident = format_ident!("__OTTER_{}_STATIC_METHODS", spec_ident);
    let prototype_methods_ident = format_ident!("__OTTER_{}_PROTOTYPE_METHODS", spec_ident);
    let prototype_accessors_ident = format_ident!("__OTTER_{}_PROTOTYPE_ACCESSORS", spec_ident);
    let constructor_rust_name = constructor.rust_name;
    let constructor_length = constructor.length;
    let static_method_entries = static_methods.iter().map(|method| {
        let rust_name = &method.rust_name;
        let js_name = &method.js_name;
        let length = &method.length;
        quote! {
            ::otter_vm::MethodSpec {
                name: #js_name,
                length: #length,
                attrs: ::otter_vm::Attr::builtin_function(),
                call: ::otter_vm::NativeCall::Static(#mod_ident::#rust_name),
            }
        }
    });
    let prototype_method_entries = prototype_methods.iter().map(|method| {
        let rust_name = &method.rust_name;
        let js_name = &method.js_name;
        let length = &method.length;
        quote! {
            ::otter_vm::MethodSpec {
                name: #js_name,
                length: #length,
                attrs: ::otter_vm::Attr::builtin_function(),
                call: ::otter_vm::NativeCall::Static(#mod_ident::#rust_name),
            }
        }
    });
    let prototype_accessor_entries = prototype_accessors.values().map(|accessor| {
        let js_name = &accessor.js_name;
        let get = accessor.get.as_ref().map_or_else(
            || quote! { None },
            |rust_name| quote! { Some(::otter_vm::NativeCall::Static(#mod_ident::#rust_name)) },
        );
        let set = accessor.set.as_ref().map_or_else(
            || quote! { None },
            |rust_name| quote! { Some(::otter_vm::NativeCall::Static(#mod_ident::#rust_name)) },
        );
        quote! {
            ::otter_vm::AccessorSpec {
                name: #js_name,
                get: #get,
                set: #set,
                attrs: ::otter_vm::Attr::new(false, false, true),
            }
        }
    });

    quote! {
        #module

        #[allow(non_upper_case_globals)]
        static #static_methods_ident: &[::otter_vm::MethodSpec] = &[
            #(#static_method_entries),*
        ];

        #[allow(non_upper_case_globals)]
        static #prototype_methods_ident: &[::otter_vm::MethodSpec] = &[
            #(#prototype_method_entries),*
        ];

        #[allow(non_upper_case_globals)]
        static #prototype_accessors_ident: &[::otter_vm::AccessorSpec] = &[
            #(#prototype_accessor_entries),*
        ];

        #[allow(non_upper_case_globals)]
        #[doc = "Generated static JavaScript class spec."]
        pub static #spec_ident: ::otter_vm::ClassSpec = ::otter_vm::ClassSpec {
            constructor: ::otter_vm::ConstructorSpec {
                name: #class_name,
                length: #constructor_length,
                call: ::otter_vm::NativeCall::Static(#mod_ident::#constructor_rust_name),
                static_methods: #static_methods_ident,
                prototype_methods: #prototype_methods_ident,
                attrs: ::otter_vm::Attr::global_binding(),
            },
            prototype_accessors: #prototype_accessors_ident,
        };
    }
    .into()
}

/// Generate a grouped static namespace spec without helper
/// attributes.
///
/// ```rust,ignore
/// otter_macros::raft! {
///     pub static MATH_SPEC: namespace("Math") {
///         methods: [
///             "abs" => math_abs, length = 1;
///         ]
///     }
/// }
/// ```
///
/// This macro is intentionally declarative: exported JS names and
/// arity remain explicit, and generated methods use
/// `NativeCall::Static`.
#[proc_macro]
pub fn raft(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as RaftInput);
    let spec_ident = input.spec_ident;
    let vis = input.vis;
    let namespace_name = input.namespace_name;
    let methods_ident = format_ident!("__OTTER_{}_METHODS", spec_ident);
    let mut seen = BTreeSet::new();
    for method in &input.methods {
        if !seen.insert(method.js_name.value()) {
            return syn::Error::new_spanned(
                &method.js_name,
                format!("duplicate raft method name `{}`", method.js_name.value()),
            )
            .to_compile_error()
            .into();
        }
    }
    let method_entries = input.methods.iter().map(|method| {
        let js_name = &method.js_name;
        let length = method.length;
        let call = &method.call;
        quote! {
            ::otter_vm::MethodSpec {
                name: #js_name,
                length: #length,
                attrs: ::otter_vm::Attr::builtin_function(),
                call: ::otter_vm::NativeCall::Static(#call),
            }
        }
    });

    quote! {
        #[allow(non_upper_case_globals)]
        static #methods_ident: &[::otter_vm::MethodSpec] = &[
            #(#method_entries),*
        ];

        #[allow(non_upper_case_globals)]
        #[doc = "Generated grouped static JavaScript namespace spec."]
        #vis static #spec_ident: ::otter_vm::NamespaceSpec = ::otter_vm::NamespaceSpec {
            name: #namespace_name,
            methods: #methods_ident,
            accessors: &[],
            constants: &[],
            attrs: ::otter_vm::Attr::global_binding(),
        };
    }
    .into()
}

struct NamespaceArgs {
    name: LitStr,
    spec: Ident,
}

impl Parse for NamespaceArgs {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let mut name = None;
        let mut spec = None;
        while !input.is_empty() {
            let key: Ident = input.parse()?;
            input.parse::<Token![=]>()?;
            match key.to_string().as_str() {
                "name" => name = Some(input.parse()?),
                "spec" => spec = Some(input.parse()?),
                other => {
                    return Err(syn::Error::new(
                        key.span(),
                        format!("unknown js_namespace argument `{other}`"),
                    ));
                }
            }
            if input.peek(Token![,]) {
                input.parse::<Token![,]>()?;
            }
        }
        Ok(Self {
            name: name.ok_or_else(|| input.error("missing `name = \"...\"`"))?,
            spec: spec.ok_or_else(|| input.error("missing `spec = IDENT`"))?,
        })
    }
}

struct ClassArgs {
    name: LitStr,
    spec: Ident,
}

struct RaftInput {
    vis: Visibility,
    spec_ident: Ident,
    namespace_name: LitStr,
    methods: Vec<RaftMethod>,
}

impl Parse for RaftInput {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let vis = input.parse()?;
        input.parse::<Token![static]>()?;
        let spec_ident = input.parse()?;
        input.parse::<Token![:]>()?;
        let kind: Ident = input.parse()?;
        if kind != "namespace" {
            return Err(syn::Error::new(
                kind.span(),
                "raft currently supports namespace specs",
            ));
        }
        let parens;
        parenthesized!(parens in input);
        let namespace_name = parens.parse()?;
        let body;
        braced!(body in input);
        let field: Ident = body.parse()?;
        if field != "methods" {
            return Err(syn::Error::new(field.span(), "expected `methods: [...]`"));
        }
        body.parse::<Token![:]>()?;
        let methods_body;
        bracketed!(methods_body in body);
        let mut methods = Vec::new();
        while !methods_body.is_empty() {
            methods.push(methods_body.parse()?);
            if methods_body.peek(Token![,]) {
                methods_body.parse::<Token![,]>()?;
            }
        }
        Ok(Self {
            vis,
            spec_ident,
            namespace_name,
            methods,
        })
    }
}

struct RaftMethod {
    js_name: LitStr,
    call: Path,
    length: u8,
}

impl Parse for RaftMethod {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let js_name = input.parse()?;
        input.parse::<Token![=>]>()?;
        let call = input.parse()?;
        input.parse::<Token![,]>()?;
        let length_key: Ident = input.parse()?;
        if length_key != "length" {
            return Err(syn::Error::new(length_key.span(), "expected `length = N`"));
        }
        input.parse::<Token![=]>()?;
        let length: LitInt = input.parse()?;
        if input.peek(Token![;]) {
            input.parse::<Token![;]>()?;
        }
        Ok(Self {
            js_name,
            call,
            length: length.base10_parse()?,
        })
    }
}

impl Parse for ClassArgs {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let args = NamespaceArgs::parse(input)?;
        Ok(Self {
            name: args.name,
            spec: args.spec,
        })
    }
}

struct FnArgs {
    name: LitStr,
    length: u8,
}

impl Parse for FnArgs {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let mut name = None;
        let mut length = None;
        while !input.is_empty() {
            let key: Ident = input.parse()?;
            input.parse::<Token![=]>()?;
            match key.to_string().as_str() {
                "name" => name = Some(input.parse()?),
                "length" => {
                    let n: LitInt = input.parse()?;
                    length = Some(n.base10_parse::<u8>()?);
                }
                other => {
                    return Err(syn::Error::new(
                        key.span(),
                        format!("unknown js_fn argument `{other}`"),
                    ));
                }
            }
            if input.peek(Token![,]) {
                input.parse::<Token![,]>()?;
            }
        }
        Ok(Self {
            name: name.ok_or_else(|| input.error("missing `name = \"...\"`"))?,
            length: length.ok_or_else(|| input.error("missing `length = N`"))?,
        })
    }
}

struct LengthArgs {
    length: u8,
}

impl Parse for LengthArgs {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let mut length = None;
        while !input.is_empty() {
            let key: Ident = input.parse()?;
            input.parse::<Token![=]>()?;
            match key.to_string().as_str() {
                "length" => {
                    let n: LitInt = input.parse()?;
                    length = Some(n.base10_parse::<u8>()?);
                }
                other => {
                    return Err(syn::Error::new(
                        key.span(),
                        format!("unknown js_constructor argument `{other}`"),
                    ));
                }
            }
            if input.peek(Token![,]) {
                input.parse::<Token![,]>()?;
            }
        }
        Ok(Self {
            length: length.ok_or_else(|| input.error("missing `length = N`"))?,
        })
    }
}

struct NameArgs {
    name: LitStr,
}

impl Parse for NameArgs {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let mut name = None;
        while !input.is_empty() {
            let key: Ident = input.parse()?;
            input.parse::<Token![=]>()?;
            match key.to_string().as_str() {
                "name" => name = Some(input.parse()?),
                other => {
                    return Err(syn::Error::new(
                        key.span(),
                        format!("unknown accessor argument `{other}`"),
                    ));
                }
            }
            if input.peek(Token![,]) {
                input.parse::<Token![,]>()?;
            }
        }
        Ok(Self {
            name: name.ok_or_else(|| input.error("missing `name = \"...\"`"))?,
        })
    }
}

struct MethodBinding {
    rust_name: Ident,
    js_name: LitStr,
    length: u8,
}

struct ConstructorBinding {
    rust_name: Ident,
    length: u8,
}

struct AccessorBinding {
    js_name: LitStr,
    get: Option<Ident>,
    set: Option<Ident>,
}

impl AccessorBinding {
    fn new(js_name: LitStr) -> Self {
        Self {
            js_name,
            get: None,
            set: None,
        }
    }
}

fn take_js_fn(attrs: &mut Vec<Attribute>) -> Result<Option<FnArgs>> {
    take_fn_attr(attrs, "js_fn")
}

fn take_fn_attr(attrs: &mut Vec<Attribute>, name: &str) -> Result<Option<FnArgs>> {
    let mut found = None;
    let mut retained = Vec::with_capacity(attrs.len());
    for attr in attrs.drain(..) {
        if !attr.path().is_ident(name) {
            retained.push(attr);
            continue;
        }
        if found.is_some() {
            return Err(syn::Error::new_spanned(
                attr,
                format!("duplicate {name} attribute on one function"),
            ));
        }
        found = Some(parse_attr_args(&attr, name)?);
    }
    *attrs = retained;
    Ok(found)
}

fn take_length_attr(attrs: &mut Vec<Attribute>, name: &str) -> Result<Option<LengthArgs>> {
    let mut found = None;
    let mut retained = Vec::with_capacity(attrs.len());
    for attr in attrs.drain(..) {
        if !attr.path().is_ident(name) {
            retained.push(attr);
            continue;
        }
        if found.is_some() {
            return Err(syn::Error::new_spanned(
                attr,
                format!("duplicate {name} attribute on one function"),
            ));
        }
        found = Some(parse_attr_args(&attr, name)?);
    }
    *attrs = retained;
    Ok(found)
}

fn take_name_attr(attrs: &mut Vec<Attribute>, name: &str) -> Result<Option<NameArgs>> {
    let mut found = None;
    let mut retained = Vec::with_capacity(attrs.len());
    for attr in attrs.drain(..) {
        if !attr.path().is_ident(name) {
            retained.push(attr);
            continue;
        }
        if found.is_some() {
            return Err(syn::Error::new_spanned(
                attr,
                format!("duplicate {name} attribute on one function"),
            ));
        }
        found = Some(parse_attr_args(&attr, name)?);
    }
    *attrs = retained;
    Ok(found)
}

fn parse_attr_args<T: Parse>(attr: &Attribute, name: &str) -> Result<T> {
    match &attr.meta {
        Meta::List(list) => list.parse_args(),
        Meta::Path(_) => Err(syn::Error::new_spanned(
            attr,
            format!("{name} requires arguments"),
        )),
        Meta::NameValue(_) => Err(syn::Error::new_spanned(
            attr,
            format!("{name} expects list-style arguments"),
        )),
    }
}
