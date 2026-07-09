//! Zero-cost JavaScript / module surface macros for Otter.
//!
//! Otter intrinsics, classes, and hosted modules are declared with a
//! family of otter-themed macros. Each macro corresponds to one role
//! in the JS / module surface; expansion produces ordinary Rust code
//! plus a `BuiltinIntrinsic`-shaped installer that bootstrap walks
//! at startup. No new runtime path, no dynamic registration — the
//! macros are pure code generation over the spec types in
//! [`otter_vm`] and the native ABI v1 documented at
//! [`docs/book/src/engine/native-call-abi.md`](../../../docs/book/src/engine/native-call-abi.md).
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
//! - [Design note](../../../docs/book/src/macros/design.md) — full
//!   surface, naming rationale, migration sequence.
//! - [Native call ABI](../../../docs/book/src/engine/native-call-abi.md)
//!   — the signature every generated method targets.
//! - [Macro overview (mdbook)](../../../docs/book/src/macros/overview.md)
//!   — narrative chapter with per-macro examples.

use std::collections::BTreeSet;

use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::parse::{Parse, ParseStream};
use syn::{
    Ident, LitInt, LitStr, Path, Result, Token, Visibility, braced, bracketed, parenthesized,
    parse_macro_input,
};

mod couch;
mod derive_from_js;
mod derive_groom;
mod derive_host_class;
mod derive_into_js;
mod derive_pelt;
mod holt;
mod js_class;
mod lodge;

/// Generate a `NamespaceSpec` + `BuiltinIntrinsic` adapter for a
/// non-constructible namespace intrinsic (`Math`, `JSON`, `Reflect`,
/// `Atomics`, `Console`, `Symbol` namespace surface, `Temporal`
/// top-level, `Intl`).
///
/// See the crate-level docs for the naming theme and full surface,
/// and [`docs/book/src/macros/design.md`](../../../docs/book/src/macros/design.md)
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

/// Declarative host-class generator — the v2 class declaration form.
///
/// Goes on an ordinary inherent `impl` block; the signatures are the
/// descriptor. Parameter types declare argument extraction
/// (`marshal::FromJs`), return types declare result construction
/// (`marshal::IntoJs`), `&self`/`&mut self` receivers declare brand
/// checks, and marker attributes carry the explicit JS names.
/// Expansion emits per-member glue plus a [`couch!`] invocation, so
/// install and the `Intrinsic` handle are the proven machinery.
///
/// # Syntax
///
/// ```rust,ignore
/// #[js_class(name = "Blob", feature = WEB)]
/// impl Blob {
///     #[constructor]
///     fn new(parts: Option<Sequence<BlobPart<'_>>>, options: Option<BlobPropertyBag>)
///         -> Result<Blob, JsError> { /* … */ }
///
///     #[getter(name = "size")]
///     fn size(&self) -> f64 { /* … */ }
///
///     #[method(name = "slice", length = 2)]
///     fn slice(&self, start: Option<f64>, end: Option<f64>) -> Blob { /* … */ }
///
///     #[method(name = "arrayBuffer", promise)]
///     fn array_buffer(&self) -> Result<marshal::ArrayBuffer, JsError> { /* … */ }
/// }
/// ```
///
/// Class options: `name = "…"` (required), `feature = …` (required),
/// `extends = Base` (native inheritance; pair with
/// `#[derive(HostClass)]` + `#[host_class(parent)]` on the data
/// struct), `tag = "…"` (`Symbol.toStringTag` override, defaults to
/// `name`). Member options are documented on the module
/// (`crates/otter-macros/src/js_class.rs`).
#[proc_macro_attribute]
pub fn js_class(attr: TokenStream, item: TokenStream) -> TokenStream {
    js_class::expand(attr, item)
}

/// Derive `marshal::HostAncestry` for a host-class data struct.
///
/// Mark one field `#[host_class(parent)]` to chain the ancestry walk
/// into a base class's data (`File` embedding `Blob`), letting
/// base-class prototype methods resolve their data on subclass
/// instances.
#[proc_macro_derive(HostClass, attributes(host_class))]
pub fn host_class_derive(input: TokenStream) -> TokenStream {
    derive_host_class::expand(input)
}

/// Derive `marshal::FromJs` for a WebIDL dictionary (named-field
/// struct) or union (enum of single-field variants). See
/// `crates/otter-macros/src/derive_from_js.rs` for member semantics
/// (`#[js(name = "…")]`, `#[js(default)]`, required members,
/// lexicographic read order, probe-ordered union variants).
#[proc_macro_derive(FromJs, attributes(js))]
pub fn from_js_derive(input: TokenStream) -> TokenStream {
    derive_from_js::expand(input)
}

/// Derive `marshal::IntoJs` for a named-field struct: builds a plain
/// object with one property per field in declaration order
/// (`#[js(name = "…")]` overrides the verbatim field name).
#[proc_macro_derive(IntoJs, attributes(js))]
pub fn into_js_derive(input: TokenStream) -> TokenStream {
    derive_into_js::expand(input)
}

/// Generate a hosted module installer + `HostedModule` row for an
/// `otter:*` / `node:*` module surface.
///
/// See the crate-level docs and
/// [`docs/book/src/macros/design.md`](../../../docs/book/src/macros/design.md)
/// for the naming theme. The macro emits:
///
/// - `pub fn install_<name>_module(&mut HostedModuleCtx) -> Result<(), String>`
/// - `pub static <UPPER>_HOSTED_MODULE: HostedModule`
///
/// Two export shapes are supported. Plain exports are static
/// `fn(ctx, args) -> Result<Value, NativeError>` pointers
/// registered through `HostedModuleCtx::builtin_method`.
/// Capability-aware exports (set `capabilities = true`) take a
/// `&CapabilitySet` snapshot captured at install time and are
/// registered through `HostedNativeCall::dynamic` with the
/// snapshot in the closure capture.
///
/// ```rust,ignore
/// otter_macros::lodge! {
///     prefix = "otter",
///     name = "kv",
///     capabilities = true,
///     exports = {
///         "openKv" / 1 => open_kv,
///         "kv"     / 1 => open_kv,
///     },
/// }
/// ```
#[proc_macro]
pub fn lodge(input: TokenStream) -> TokenStream {
    lodge::expand(input)
}

/// Derive `otter_gc::SafeTraceable` for a GC body struct.
///
/// One field-level call per non-`#[pelt(skip)]` field; missing
/// `PeltField` impls surface at the field's span. The struct must
/// carry a `#[pelt(tag = <CONST>)]` attribute that names the
/// `Traceable::TYPE_TAG` constant — by convention the same
/// per-body `<NAME>_TYPE_TAG` const the hand-written installers
/// already declare.
///
/// # Syntax
///
/// ```rust,ignore
/// use otter_macros::Pelt;
///
/// #[derive(Pelt)]
/// #[pelt(tag = PROXY_BODY_TYPE_TAG)]
/// pub struct ProxyBodyGc {
///     pub target: otter_vm::Value,
///     pub handler: otter_vm::Value,
///     #[pelt(skip)] // primitive — not a GC slot
///     pub revoked: bool,
/// }
/// ```
///
/// # Generated impl
///
/// ```rust,ignore
/// impl otter_gc::SafeTraceable for ProxyBodyGc {
///     const TYPE_TAG: u8 = PROXY_BODY_TYPE_TAG;
///     fn trace_slots_safe(&self, visitor: &mut otter_gc::raw::SlotVisitor<'_>) {
///         otter_vm::pelt::PeltField::pelt_trace(&self.target, visitor);
///         otter_vm::pelt::PeltField::pelt_trace(&self.handler, visitor);
///     }
/// }
/// ```
///
/// Per-field tracers that need custom logic (Cell-shaped interior
/// mutability, ephemeron entries, etc.) keep their hand-written
/// `SafeTraceable` impl. See [`docs/book/src/macros/design.md`] for the
/// full surface and the planned `#[pelt(via = path)]` extension.
#[proc_macro_derive(Pelt, attributes(pelt))]
pub fn pelt_derive(input: TokenStream) -> TokenStream {
    derive_pelt::expand(input)
}

/// Derive `::otter_gc::SafeFinalize` for a GC body.
///
/// Mirrors [`Pelt`] for the sweep-time finalize hook: every field
/// that is not annotated with `#[groom(skip)]` is funneled through
/// `::otter_vm::groom::GroomField::groom`, in declaration order.
///
/// Bodies that opt into `Groom` must also implement
/// `::otter_gc::SafeTraceable` (typically via `#[derive(Pelt)]`) and
/// register the finalize wrapper once with the host heap:
///
/// ```rust,ignore
/// heap.register_finalize::<MyBody>();
/// ```
///
/// See [`docs/book/src/macros/design.md`](../../../docs/book/src/macros/design.md)
/// for the full Pelt / Groom surface.
#[proc_macro_derive(Groom, attributes(groom))]
pub fn groom_derive(input: TokenStream) -> TokenStream {
    derive_groom::expand(input)
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
