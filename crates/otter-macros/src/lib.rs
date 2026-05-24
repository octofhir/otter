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
//! `docs/architecture-refactor-plan-2026-05.md` Task 4.1. Sub-phase
//! 4.1b deleted the legacy `#[js_namespace]` / `#[js_class]` /
//! `#[js_fn]` / `#[js_constructor]` attribute macros; only the
//! otter-themed surface (`holt!`, `couch!`, `raft!`) remains.

use std::collections::BTreeSet;

use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::parse::{Parse, ParseStream};
use syn::{
    Ident, LitInt, LitStr, Path, Result, Token, Visibility, braced, bracketed, parenthesized,
    parse_macro_input,
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

/// Emit a `&[MethodSpec]` table plus a `pub static <SPEC>:
/// NamespaceSpec` that wraps it. Used standalone when assembling a
/// method table by hand; inline rows inside `holt!` / `couch!`
/// generate equivalent statics directly, so most production sites
/// don't need this.
///
/// ```rust,ignore
/// otter_macros::raft! {
///     pub static MY_SPEC: namespace("MyThing") {
///         methods: [
///             "foo" => path::to::foo, length = 0,
///             "bar" => path::to::bar, length = 1,
///         ]
///     }
/// }
/// ```
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

