//! `couch!` proc macro — class intrinsic generator.
//!
//! See the crate-level docs and
//! [`docs/book/src/macros/design.md`](../../../docs/book/src/macros/design.md)
//! for the naming theme and the full surface. This module owns the
//! parsing + expansion for the `couch!` invocation only.
//!
//! # Contents
//! - [`expand`] — entry called from the proc-macro shim in
//!   [`crate::couch`](super) re-exported as `couch!`.
//! - [`CouchInput`] — parsed top-level fields.
//! - [`ConstructorSpecArgs`] — parsed `constructor = (length = N, call = path)`
//!   tuple.
//!
//! # Surface
//!
//! Minimum invocation:
//!
//! ```rust,ignore
//! couch! {
//!     name = "Proxy",
//!     feature = CORE,
//!     constructor = (length = 2, call = proxy_ctor_call),
//! }
//! ```
//!
//! Optional fields:
//!
//! - `constructor = (length = N, call = path
//!     [, callable_only = true]
//!     [, is_abstract = true])` — `callable_only = true` drops the
//!   `[[Construct]]` slot so `new Foo(x)` throws via §10.1.10
//!   (Symbol / BigInt / Number / Boolean / String). `is_abstract`
//!   documents intent for things like `%TypedArray%`; the install
//!   path is unchanged.
//! - `statics = { "name" / length => path, ... }` — inline rows
//!   pinned as own data properties on the constructor
//!   (`Proxy.revocable`).
//! - `static_method_specs = [path::TO_SLICE, ...]` — references to
//!   pre-built `&[MethodSpec]` slices pinned as own data properties
//!   on the constructor. Used when the same slice is also consumed
//!   elsewhere (e.g. `Op::CallMethod` intrinsic dispatch fast path
//!   for `String.fromCharCode`).
//! - `static_constants = [("NAME", Kind(expr) [, attrs]), ...]` —
//!   numeric / boolean / nullish constants pinned as own data
//!   properties on the constructor (`Number.MAX_VALUE`,
//!   `Number.NaN`). `Kind` is one of `Undefined`, `Null`, `Boolean`,
//!   `Number`. Defaults to `Attr::read_only()` per §21.1.2.
//! - `prototype = { methods = { ... }, accessors = [...],
//!     method_specs = [...], parent = path }` — the prototype block.
//!   Inline `methods` rows generate a `&[MethodSpec]` slice;
//!   `method_specs` accepts pre-built slice paths (parallel to
//!   `static_method_specs`). The optional `parent = path` overrides
//!   the default `%Object.prototype%` link with whatever
//!   `path(global, heap) -> JsObject` returns — used by per-kind
//!   TypedArrays that chain to `%TypedArray%.prototype`.
//! - `no_prototype = true` — suppresses the constructor's own
//!   `.prototype` property for spec outliers such as `%Proxy%`.
//! - `prototype_constants = [("NAME", Kind(expr) [, attrs]), ...]` —
//!   mirrors `static_constants` but pins on the prototype. Used for
//!   `TypedArray.prototype.BYTES_PER_ELEMENT` per §23.2.6.1.
//! - `ctor_parent = path` — resolver fn for the constructor's
//!   `[[Prototype]]` override. `path(global, heap) -> Value`. Used
//!   by per-kind TypedArrays to inherit from `%TypedArray%`.
//! - `install_on = path` — resolver fn for the parent host object
//!   the constructor binds on. `path(global, heap) -> JsObject`.
//!   Without it, the constructor binds on `globalThis`. Used for
//!   nested ctors (e.g. `Temporal.Instant`, `Temporal.Duration`).
//!   Used for nested ctors (e.g. `Temporal.Instant`,
//!   `Temporal.Duration`) — see [crate::holt::HoltInput].
//! - `post_install = path` — escape hatch. When set, the generated
//!   install body calls `path(heap, global, ctor)?` after pinning
//!   the constructor on `globalThis`. Used for things that don't
//!   fit declarative rows: setting hidden internal slots on the
//!   prototype (e.g. `[[BooleanData]] = false`), legacy accessors
//!   that need captures bound to the ctor identity (`RegExp.input`
//!   / `$_` / `$1`..`$9`), identity-shared globals
//!   (`Number.parseInt === globalThis.parseInt`).
//! - `spec = MY_SPEC,` — override the derived `<NAME>_SPEC` ident.
//! - `intrinsic = MyIntrinsic,` — override the default `Intrinsic`
//!   ident.
//!
//! # Generated symbols
//!
//! - `pub static <NAME>_SPEC: ::otter_vm::ConstructorSpec` — the
//!   raw constructor spec (metadata + inline static methods +
//!   inline prototype methods).
//! - `pub struct <INTRINSIC>;` + `impl BuiltinIntrinsic for
//!   <INTRINSIC>` whose `install` body:
//!   1. allocates the constructor via
//!      `bootstrap::native_constructor_static_with_value_roots`
//!      (or `native_static_with_value_roots` when
//!      `callable_only = true`),
//!   2. pins each `statics` row + `static_method_specs` slice +
//!      `static_constants` row as own properties on the ctor,
//!      3. if the prototype block has any content, allocates the
//!      prototype, links it to `%Object.prototype%`, installs the
//!      prototype methods (inline + slice) + accessors through the
//!      `bootstrap::*_with_value_roots` allocators (which thread the
//!      prototype + ctor as rooted values by reference, so no raw
//!      handle is read across an allocation), attaches it on the
//!      ctor's `prototype` slot (non-writable / non-enumerable /
//!      non-configurable), and pins the `prototype.constructor = ctor`
//!      back-pointer (writable / non-enumerable / configurable),
//!   4. binds the ctor on `globalThis` through
//!      `bootstrap::define_global_value`,
//!   5. calls `post_install(heap, global, ctor)?` when supplied.
//!
//! # Out of scope
//!
//! Cross-class fixups that depend on the per-realm
//! `WellKnownSymbols` table (`@@toStringTag`, `@@iterator`, species
//! accessors) do **not** ride `couch!`; they stay in dedicated
//! `install_<class>_well_knowns_post_bootstrap` hooks that
//! bootstrap calls after the symbol table is materialised. The
//! shared abstract-prototype shape used by TypedArrays (one
//! abstract `%TypedArray%.prototype` with 11 per-kind prototypes
//! chained to it) is also out of scope; that surface uses a
//! hand-written installer.
//!
//! # See also
//! - [`crate::holt`](super::holt) — namespace intrinsic generator
//!   sharing the [`super::holt::MethodEntry`] grammar.

use proc_macro::TokenStream;
use proc_macro2::Span;
use quote::{format_ident, quote};
use std::collections::BTreeSet;
use syn::parse::{Parse, ParseStream};
use syn::{
    Ident, LitBool, LitInt, LitStr, Path, Result, Token, braced, bracketed, parenthesized,
    parse_macro_input,
};

use crate::holt::{AccessorEntry, ConstantEntry, MethodEntry};

/// Parsed `constructor = (length = N, call = path
/// [, is_abstract = true] [, callable_only = true])` tuple.
pub(crate) struct ConstructorSpecArgs {
    pub(crate) length: u8,
    pub(crate) call: Path,
    pub(crate) is_abstract: bool,
    /// When `true`, the ctor lacks a `[[Construct]]` slot — the
    /// install body allocates the function through
    /// `bootstrap::native_static_with_value_roots` instead of
    /// `bootstrap::native_constructor_static_with_value_roots`.
    /// Matches the §21.1.1 (`Number`) / §21.2.1 (`BigInt`) /
    /// §22.1.1 (`String`) / §20.4.1 (`Symbol`) / §20.3.1
    /// (`Boolean`) shape where `new Foo(x)` throws "is not a
    /// constructor" via §10.1.10.
    pub(crate) callable_only: bool,
}

impl Parse for ConstructorSpecArgs {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let body;
        parenthesized!(body in input);
        let mut length: Option<u8> = None;
        let mut call: Option<Path> = None;
        let mut is_abstract = false;
        let mut callable_only = false;
        while !body.is_empty() {
            let key: Ident = body.parse()?;
            body.parse::<Token![=]>()?;
            match key.to_string().as_str() {
                "length" => {
                    let lit: LitInt = body.parse()?;
                    length = Some(lit.base10_parse()?);
                }
                "call" => {
                    call = Some(body.parse()?);
                }
                "is_abstract" => {
                    let lit: LitBool = body.parse()?;
                    is_abstract = lit.value;
                }
                "callable_only" => {
                    let lit: LitBool = body.parse()?;
                    callable_only = lit.value;
                }
                other => {
                    return Err(syn::Error::new(
                        key.span(),
                        format!(
                            "couch!: unknown constructor field `{other}` — expected \
                             `length`, `call`, `is_abstract`, or `callable_only` \
                             (Rust reserves the bare `abstract` keyword)"
                        ),
                    ));
                }
            }
            if body.peek(Token![,]) {
                body.parse::<Token![,]>()?;
            }
        }
        Ok(Self {
            length: length.ok_or_else(|| {
                syn::Error::new(
                    Span::call_site(),
                    "couch!: constructor block missing `length = N`",
                )
            })?,
            call: call.ok_or_else(|| {
                syn::Error::new(
                    Span::call_site(),
                    "couch!: constructor block missing `call = path::to::fn`",
                )
            })?,
            is_abstract,
            callable_only,
        })
    }
}

/// Parsed `prototype = { methods = { ... }, accessors = [...],
/// method_specs = [path, ...] }` block.
///
/// `methods` and `accessors` parse inline rows; `method_specs`
/// references pre-built `&[MethodSpec]` slices (used when a
/// `BuiltinIntrinsic` has many prototype methods declared through a
/// generator macro that emits one `pub static FOO: &[MethodSpec] =
/// &[...]` slice). The install body iterates each slice in order.
#[derive(Default)]
pub(crate) struct PrototypeBlock {
    pub(crate) methods: Vec<MethodEntry>,
    pub(crate) accessors: Vec<AccessorEntry>,
    pub(crate) method_specs: Vec<Path>,
    /// Optional `parent = path` override. When set, the install body
    /// resolves the prototype's `[[Prototype]]` via
    /// `path(global, heap)` instead of linking to `%Object.prototype%`.
    /// Used by TypedArray per-kind prototypes that chain to
    /// `%TypedArray%.prototype` per §23.2.6.
    pub(crate) parent: Option<Path>,
}

impl Parse for PrototypeBlock {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let body;
        braced!(body in input);
        let mut methods: Vec<MethodEntry> = Vec::new();
        let mut accessors: Vec<AccessorEntry> = Vec::new();
        let mut method_specs: Vec<Path> = Vec::new();
        let mut parent: Option<Path> = None;
        while !body.is_empty() {
            let key: Ident = body.parse()?;
            body.parse::<Token![=]>()?;
            match key.to_string().as_str() {
                "parent" => {
                    parent = Some(body.parse()?);
                }
                "methods" => {
                    let methods_body;
                    braced!(methods_body in body);
                    while !methods_body.is_empty() {
                        methods.push(methods_body.parse()?);
                        if methods_body.peek(Token![,]) {
                            methods_body.parse::<Token![,]>()?;
                        }
                    }
                }
                "accessors" => {
                    let accessors_body;
                    bracketed!(accessors_body in body);
                    while !accessors_body.is_empty() {
                        accessors.push(accessors_body.parse()?);
                        if accessors_body.peek(Token![,]) {
                            accessors_body.parse::<Token![,]>()?;
                        }
                    }
                }
                "method_specs" => {
                    let specs_body;
                    bracketed!(specs_body in body);
                    while !specs_body.is_empty() {
                        method_specs.push(specs_body.parse()?);
                        if specs_body.peek(Token![,]) {
                            specs_body.parse::<Token![,]>()?;
                        }
                    }
                }
                other => {
                    return Err(syn::Error::new(
                        key.span(),
                        format!(
                            "couch! prototype: unknown field `{other}` — expected \
                             `methods`, `accessors`, `method_specs`, or `parent`"
                        ),
                    ));
                }
            }
            if body.peek(Token![,]) {
                body.parse::<Token![,]>()?;
            }
        }
        Ok(Self {
            methods,
            accessors,
            method_specs,
            parent,
        })
    }
}

/// Parsed body of a `couch!` invocation.
pub(crate) struct CouchInput {
    pub(crate) name: LitStr,
    pub(crate) feature: Ident,
    pub(crate) spec_ident: Ident,
    pub(crate) intrinsic_ident: Ident,
    pub(crate) constructor: ConstructorSpecArgs,
    pub(crate) statics: Vec<MethodEntry>,
    /// References to pre-built `&[MethodSpec]` slices pinned as own data
    /// properties on the constructor. Mirrors `prototype.method_specs`
    /// for cases where the static surface is declared via a separate
    /// generator macro (e.g. `STRING_STATIC_METHODS`).
    pub(crate) static_method_specs: Vec<Path>,
    /// Numeric / boolean / nullish constants pinned as own data
    /// properties on the constructor itself (e.g. `Number.MAX_VALUE`,
    /// `Math.PI` — though Math is a namespace, not a class). Shares
    /// the `holt!` constant grammar.
    pub(crate) static_constants: Vec<ConstantEntry>,
    pub(crate) prototype: PrototypeBlock,
    /// Mirrors [`Self::static_constants`] but pins the constants on
    /// the **prototype** instead of the constructor. Used for
    /// `TypedArray.prototype.BYTES_PER_ELEMENT` per §23.2.6.1.
    pub(crate) prototype_constants: Vec<ConstantEntry>,
    /// Suppress the constructor's own `.prototype` property. Most
    /// constructors have one; `%Proxy%` is the spec outlier.
    pub(crate) no_prototype: bool,
    /// Optional `ctor_parent = path` override. When set, the install
    /// body resolves the constructor's `[[Prototype]]` override via
    /// `path(global, heap) -> Value`. Used for concrete TypedArray
    /// constructors that inherit from `%TypedArray%` per §23.2.6.1.
    pub(crate) ctor_parent: Option<Path>,
    /// Optional `install_on = path` parent override. When set, the
    /// generated install body looks up the parent host object via
    /// `path(global, heap)` and binds the constructor there instead
    /// of on `globalThis`. Used for nested class ctors that live
    /// under a namespace object (e.g. `Temporal.Instant`,
    /// `Temporal.Duration`).
    pub(crate) install_on: Option<Path>,
    /// Optional `post_install = path` escape hatch. When set, the
    /// generated install body calls
    /// `path(heap, global, ctor)?` after pinning the constructor on
    /// `globalThis`. Used for things that don't fit the declarative
    /// rows (e.g. legacy `RegExp` static accessors that need captures
    /// to bind the constructor identity).
    pub(crate) post_install: Option<Path>,
    /// Optional `string_tag = "Intl.Locale"` — installs
    /// `prototype[@@toStringTag]` (non-enumerable, configurable) at
    /// construction time through
    /// [`BuiltinIntrinsic::install_well_knowns`], avoiding a
    /// post-bootstrap fixup pass.
    pub(crate) string_tag: Option<LitStr>,
}

impl Parse for CouchInput {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let mut name: Option<LitStr> = None;
        let mut feature: Option<Ident> = None;
        let mut spec_override: Option<Ident> = None;
        let mut intrinsic_override: Option<Ident> = None;
        let mut constructor: Option<ConstructorSpecArgs> = None;
        let mut statics: Vec<MethodEntry> = Vec::new();
        let mut static_method_specs: Vec<Path> = Vec::new();
        let mut static_constants: Vec<ConstantEntry> = Vec::new();
        let mut prototype_constants: Vec<ConstantEntry> = Vec::new();
        let mut prototype: PrototypeBlock = PrototypeBlock::default();
        let mut no_prototype = false;
        let mut ctor_parent: Option<Path> = None;
        let mut install_on: Option<Path> = None;
        let mut post_install: Option<Path> = None;
        let mut string_tag: Option<LitStr> = None;

        while !input.is_empty() {
            let key: Ident = input.parse()?;
            input.parse::<Token![=]>()?;
            match key.to_string().as_str() {
                "name" => name = Some(input.parse()?),
                "feature" => feature = Some(input.parse()?),
                "spec" => spec_override = Some(input.parse()?),
                "intrinsic" => intrinsic_override = Some(input.parse()?),
                "constructor" => constructor = Some(input.parse()?),
                "statics" => {
                    let body;
                    braced!(body in input);
                    while !body.is_empty() {
                        statics.push(body.parse()?);
                        if body.peek(Token![,]) {
                            body.parse::<Token![,]>()?;
                        }
                    }
                }
                "static_method_specs" => {
                    let body;
                    bracketed!(body in input);
                    while !body.is_empty() {
                        static_method_specs.push(body.parse()?);
                        if body.peek(Token![,]) {
                            body.parse::<Token![,]>()?;
                        }
                    }
                }
                "static_constants" => {
                    let body;
                    bracketed!(body in input);
                    while !body.is_empty() {
                        static_constants.push(body.parse()?);
                        if body.peek(Token![,]) {
                            body.parse::<Token![,]>()?;
                        }
                    }
                }
                "prototype_constants" => {
                    let body;
                    bracketed!(body in input);
                    while !body.is_empty() {
                        prototype_constants.push(body.parse()?);
                        if body.peek(Token![,]) {
                            body.parse::<Token![,]>()?;
                        }
                    }
                }
                "prototype" => {
                    prototype = input.parse()?;
                }
                "no_prototype" => {
                    let lit: LitBool = input.parse()?;
                    no_prototype = lit.value;
                }
                "ctor_parent" => {
                    ctor_parent = Some(input.parse()?);
                }
                "install_on" => {
                    install_on = Some(input.parse()?);
                }
                "post_install" => {
                    post_install = Some(input.parse()?);
                }
                "string_tag" => {
                    string_tag = Some(input.parse()?);
                }
                other => {
                    return Err(syn::Error::new(
                        key.span(),
                        format!(
                            "unknown `couch!` field `{other}` — expected `name`, `feature`, \
                             `spec`, `intrinsic`, `constructor`, `statics`, `static_method_specs`, \
                             `static_constants`, `prototype`, `no_prototype`, `ctor_parent`, \
                             `install_on`, `post_install`, or `string_tag`"
                        ),
                    ));
                }
            }
            if input.peek(Token![,]) {
                input.parse::<Token![,]>()?;
            }
        }

        let name = name.ok_or_else(|| {
            syn::Error::new(Span::call_site(), "couch!: missing `name = \"...\"`")
        })?;
        let feature = feature.ok_or_else(|| {
            syn::Error::new(
                Span::call_site(),
                "couch!: missing `feature = <BootstrapFeatures variant>` (e.g. `feature = CORE`)",
            )
        })?;
        let constructor = constructor.ok_or_else(|| {
            syn::Error::new(
                Span::call_site(),
                "couch!: missing `constructor = (length = N, call = path::to::fn)`",
            )
        })?;

        let spec_ident = spec_override.unwrap_or_else(|| {
            let upper = name.value().to_ascii_uppercase();
            let sanitized: String = upper
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
                .collect();
            format_ident!("{}_SPEC", sanitized, span = name.span())
        });
        let intrinsic_ident =
            intrinsic_override.unwrap_or_else(|| Ident::new("Intrinsic", Span::call_site()));

        Ok(Self {
            name,
            feature,
            spec_ident,
            intrinsic_ident,
            constructor,
            statics,
            static_method_specs,
            static_constants,
            prototype_constants,
            no_prototype,
            prototype,
            ctor_parent,
            install_on,
            post_install,
            string_tag,
        })
    }
}

pub(crate) fn expand(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as CouchInput);
    let CouchInput {
        name,
        feature,
        spec_ident,
        intrinsic_ident,
        constructor,
        statics,
        static_method_specs,
        static_constants,
        prototype_constants,
        no_prototype,
        prototype,
        ctor_parent,
        install_on,
        post_install,
        string_tag,
    } = input;

    let mut seen = BTreeSet::new();
    for m in &statics {
        if !seen.insert(m.js_name.value()) {
            return syn::Error::new_spanned(
                &m.js_name,
                format!("couch!: duplicate static name `{}`", m.js_name.value()),
            )
            .to_compile_error()
            .into();
        }
    }
    let mut seen_proto_m = BTreeSet::new();
    for m in &prototype.methods {
        if !seen_proto_m.insert(m.js_name.value()) {
            return syn::Error::new_spanned(
                &m.js_name,
                format!("couch!: duplicate prototype method `{}`", m.js_name.value()),
            )
            .to_compile_error()
            .into();
        }
    }
    let mut seen_proto_a = BTreeSet::new();
    for a in &prototype.accessors {
        if !seen_proto_a.insert(a.js_name.value()) {
            return syn::Error::new_spanned(
                &a.js_name,
                format!(
                    "couch!: duplicate prototype accessor `{}`",
                    a.js_name.value()
                ),
            )
            .to_compile_error()
            .into();
        }
    }

    let statics_ident = format_ident!("__OTTER_{}_STATICS", spec_ident);
    let static_constants_ident = format_ident!("__OTTER_{}_STATIC_CONSTANTS", spec_ident);
    let prototype_constants_ident = format_ident!("__OTTER_{}_PROTOTYPE_CONSTANTS", spec_ident);
    let prototype_methods_ident = format_ident!("__OTTER_{}_PROTOTYPE_METHODS", spec_ident);
    let prototype_accessors_ident = format_ident!("__OTTER_{}_PROTOTYPE_ACCESSORS", spec_ident);
    let static_entries = statics.iter().map(|m| {
        let js_name = &m.js_name;
        let length = m.length;
        let call = &m.call;
        let attrs_path = crate::holt::attrs_factory_path(m.attrs.as_ref(), "builtin_function");
        quote! {
            ::otter_vm::MethodSpec {
                name: #js_name,
                length: #length,
                attrs: #attrs_path,
                call: ::otter_vm::NativeCall::Static(#call),
            }
        }
    });
    let prototype_method_entries = prototype.methods.iter().map(|m| {
        let js_name = &m.js_name;
        let length = m.length;
        let call = &m.call;
        let attrs_path = crate::holt::attrs_factory_path(m.attrs.as_ref(), "builtin_function");
        quote! {
            ::otter_vm::MethodSpec {
                name: #js_name,
                length: #length,
                attrs: #attrs_path,
                call: ::otter_vm::NativeCall::Static(#call),
            }
        }
    });
    let prototype_constant_entries = prototype_constants.iter().map(|c| {
        let js_name = &c.js_name;
        let attrs_path = crate::holt::attrs_factory_path(c.attrs.as_ref(), "read_only");
        let value_tokens = match (c.kind.to_string().as_str(), c.value.as_ref()) {
            ("Undefined", None) => quote! { ::otter_vm::ConstValue::Undefined },
            ("Null", None) => quote! { ::otter_vm::ConstValue::Null },
            ("Boolean", Some(expr)) => quote! { ::otter_vm::ConstValue::Boolean(#expr) },
            ("Number", Some(expr)) => quote! { ::otter_vm::ConstValue::Number(#expr) },
            ("Undefined" | "Null", Some(_)) => {
                return syn::Error::new_spanned(
                    &c.kind,
                    format!(
                        "couch!: `{}` prototype constant takes no value — drop the `(expr)` suffix",
                        c.kind
                    ),
                )
                .to_compile_error();
            }
            ("Boolean" | "Number", None) => {
                return syn::Error::new_spanned(
                    &c.kind,
                    format!(
                        "couch!: `{}` prototype constant requires a `(expr)` value",
                        c.kind
                    ),
                )
                .to_compile_error();
            }
            (other, _) => {
                return syn::Error::new_spanned(
                    &c.kind,
                    format!(
                        "couch!: unknown prototype constant kind `{other}` — expected one of \
                         `Undefined`, `Null`, `Boolean`, `Number`"
                    ),
                )
                .to_compile_error();
            }
        };
        quote! {
            ::otter_vm::ConstSpec {
                name: #js_name,
                value: #value_tokens,
                attrs: #attrs_path,
            }
        }
    });

    let static_constant_entries = static_constants.iter().map(|c| {
        let js_name = &c.js_name;
        let attrs_path = crate::holt::attrs_factory_path(c.attrs.as_ref(), "read_only");
        let value_tokens = match (c.kind.to_string().as_str(), c.value.as_ref()) {
            ("Undefined", None) => quote! { ::otter_vm::ConstValue::Undefined },
            ("Null", None) => quote! { ::otter_vm::ConstValue::Null },
            ("Boolean", Some(expr)) => quote! { ::otter_vm::ConstValue::Boolean(#expr) },
            ("Number", Some(expr)) => quote! { ::otter_vm::ConstValue::Number(#expr) },
            ("Undefined" | "Null", Some(_)) => {
                return syn::Error::new_spanned(
                    &c.kind,
                    format!(
                        "couch!: `{}` static constant takes no value — drop the `(expr)` suffix",
                        c.kind
                    ),
                )
                .to_compile_error();
            }
            ("Boolean" | "Number", None) => {
                return syn::Error::new_spanned(
                    &c.kind,
                    format!(
                        "couch!: `{}` static constant requires a `(expr)` value",
                        c.kind
                    ),
                )
                .to_compile_error();
            }
            (other, _) => {
                return syn::Error::new_spanned(
                    &c.kind,
                    format!(
                        "couch!: unknown static constant kind `{other}` — expected one of \
                         `Undefined`, `Null`, `Boolean`, `Number`"
                    ),
                )
                .to_compile_error();
            }
        };
        quote! {
            ::otter_vm::ConstSpec {
                name: #js_name,
                value: #value_tokens,
                attrs: #attrs_path,
            }
        }
    });

    let prototype_accessor_entries = prototype.accessors.iter().map(|a| {
        let js_name = &a.js_name;
        let get_tokens = match &a.get {
            Some(path) => quote! {
                ::core::option::Option::Some(::otter_vm::NativeCall::Static(#path))
            },
            None => quote! { ::core::option::Option::None },
        };
        let set_tokens = match &a.set {
            Some(path) => quote! {
                ::core::option::Option::Some(::otter_vm::NativeCall::Static(#path))
            },
            None => quote! { ::core::option::Option::None },
        };
        let attrs_path = crate::holt::attrs_factory_path(a.attrs.as_ref(), "builtin_function");
        quote! {
            ::otter_vm::AccessorSpec {
                name: #js_name,
                get_name: ::core::concat!("get ", #js_name),
                set_name: ::core::concat!("set ", #js_name),
                get: #get_tokens,
                set: #set_tokens,
                attrs: #attrs_path,
            }
        }
    });

    let ctor_length = constructor.length;
    let ctor_call = &constructor.call;
    let feature_path = quote! { ::otter_vm::bootstrap::BootstrapFeatures::#feature };
    // Abstract ctors still wire their `call` field (for diagnostics
    // + name resolution), but the macro emits the same install
    // path; the user's call body is expected to throw a TypeError.
    // The flag mostly documents intent today; future expansions may
    // synthesise a default throw stub when `abstract = true` and no
    // `call` is supplied.
    let _ = constructor.is_abstract;
    // Callable-only ctors lose their [[Construct]] slot, so `new
    // BigInt(x)` throws via §10.1.10 instead of via the body.
    let ctor_alloc_path = if constructor.callable_only {
        quote! { ::otter_vm::bootstrap::native_static_with_value_roots }
    } else {
        quote! { ::otter_vm::bootstrap::native_constructor_static_with_value_roots }
    };

    let static_method_spec_iters = static_method_specs.iter().map(|path| {
        quote! {
            for method_spec in #path.iter() {
                let call_target = match method_spec.call {
                    ::otter_vm::NativeCall::Static(f) => f,
                    ::otter_vm::NativeCall::VmIntrinsic(_)
                    | ::otter_vm::NativeCall::Dynamic(_) => {
                        return ::core::result::Result::Err(
                            ::otter_vm::JsSurfaceError::DefinePropertyFailed(method_spec.name),
                        );
                    }
                };
                let fn_obj = ::otter_vm::bootstrap::native_static_with_value_roots(
                    heap,
                    method_spec.name,
                    method_spec.length,
                    call_target,
                    &[&global_root, &ctor_value],
                )
                .map_err(::otter_vm::JsSurfaceError::from)?;
                let desc = ::otter_vm::object::PropertyDescriptor::data(
                    ::otter_vm::Value::native_function(fn_obj),
                    method_spec.attrs.writable,
                    method_spec.attrs.enumerable,
                    method_spec.attrs.configurable,
                );
                let ctor = ctor_value
                    .as_native_function()
                    .expect("couch!: constructor stays a native function across allocation");
                if !ctor.define_own_property(heap, method_spec.name, desc) {
                    return ::core::result::Result::Err(
                        ::otter_vm::JsSurfaceError::DefinePropertyFailed(method_spec.name),
                    );
                }
            }
        }
    });
    // Prototype method-spec slices install by reference, mirroring the inline
    // prototype-method loop: every allocation threads the rooted `_value` copies,
    // so `prototype_value` stays current and no raw handle is read across an
    // allocation. `native_from_call_with_value_roots` accepts every `NativeCall`
    // variant, including the `VmIntrinsic` fast-path targets.
    let extra_method_spec_iters = prototype.method_specs.iter().map(|path| {
        quote! {
            for method_spec in #path.iter() {
                let fn_obj = ::otter_vm::bootstrap::native_from_call_with_value_roots(
                    heap,
                    method_spec.name,
                    method_spec.length,
                    method_spec.call.clone(),
                    &[&global_root, &ctor_value, &prototype_value],
                )
                .map_err(::otter_vm::JsSurfaceError::from)?;
                let desc = ::otter_vm::object::PropertyDescriptor::data(
                    ::otter_vm::Value::native_function(fn_obj),
                    method_spec.attrs.writable,
                    method_spec.attrs.enumerable,
                    method_spec.attrs.configurable,
                );
                let prototype = prototype_value
                    .as_object()
                    .expect("couch!: prototype stays an object across allocation");
                if !::otter_vm::object::define_own_property(
                    prototype,
                    heap,
                    method_spec.name,
                    desc,
                ) {
                    return ::core::result::Result::Err(
                        ::otter_vm::JsSurfaceError::DefinePropertyFailed(method_spec.name),
                    );
                }
            }
        }
    });
    // Nearly every builtin constructor exposes a `.prototype` data
    // property; `%Proxy%` is the named exception (§28.2.1).
    let prototype_block_needed = !no_prototype;

    let post_install_call = match post_install {
        Some(path) => quote! {
            // Resolve the constructor through its rooted `Value`: the install
            // allocations may have relocated it, and only `ctor_value` was kept
            // current.
            let ctor = ctor_value
                .as_native_function()
                .expect("couch!: constructor stays a native function across allocation");
            #path(heap, global, ctor)?;
        },
        None => quote! {},
    };

    let prototype_parent_link = match &prototype.parent {
        Some(path) => quote! {
            // Custom prototype parent — caller provides a resolver
            // that returns the JsObject to link `[[Prototype]]` to.
            let parent_proto = #path(global, heap);
            ::otter_vm::object::set_prototype(
                prototype,
                heap,
                ::core::option::Option::Some(parent_proto),
            );
        },
        None => quote! {
            // Default — link to `%Object.prototype%` per §19.4 when
            // Object is already installed.
            if let ::core::option::Option::Some(object_ctor) =
                ::otter_vm::object::get(global, heap, "Object")
                    .and_then(|v| v.as_object())
                && let ::core::option::Option::Some(object_proto) =
                    ::otter_vm::object::get(object_ctor, heap, "prototype")
                        .and_then(|v| v.as_object())
            {
                ::otter_vm::object::set_prototype(
                    prototype,
                    heap,
                    ::core::option::Option::Some(object_proto),
                );
            } else if let ::core::option::Option::Some(object_ctor_value) =
                ::otter_vm::object::get(global, heap, "Object")
                && let ::core::option::Option::Some(object_ctor_native) =
                    object_ctor_value.as_native_function()
                && let ::core::result::Result::Ok(::core::option::Option::Some(desc)) =
                    object_ctor_native.own_property_descriptor(heap, "prototype")
                && let ::otter_vm::object::DescriptorKind::Data { value } = desc.kind
                && let ::core::option::Option::Some(object_proto) = value.as_object()
            {
                ::otter_vm::object::set_prototype(
                    prototype,
                    heap,
                    ::core::option::Option::Some(object_proto),
                );
            }
        },
    };

    let ctor_parent_link = match ctor_parent {
        Some(path) => quote! {
            // Custom constructor `[[Prototype]]` override — used by
            // concrete TypedArray ctors that inherit from
            // `%TypedArray%` per §23.2.6.1.
            let parent_ctor = #path(global, heap);
            ctor.set_prototype_override(heap, ::core::option::Option::Some(parent_ctor));
        },
        None => quote! {},
    };

    // `string_tag = "..."` — install `prototype[@@toStringTag]` at
    // construction time (no post-bootstrap fixup). Resolves the
    // constructor off the same host as `install`, reads its prototype,
    // and pins the tag as a non-enumerable, configurable data property.
    let install_well_knowns_fn = match &string_tag {
        Some(tag) => {
            let host_expr = match &install_on {
                Some(path) => quote! { #path(global, heap) },
                None => quote! { global },
            };
            quote! {
                fn install_well_knowns(
                    heap: &mut ::otter_gc::GcHeap,
                    global: ::otter_vm::JsObject,
                    well_known: &::otter_vm::symbol::WellKnownSymbols,
                ) -> ::core::result::Result<(), ::otter_vm::JsSurfaceError> {
                    let host = #host_expr;
                    let ::core::option::Option::Some(ctor) =
                        ::otter_vm::object::get(host, heap, #name)
                    else {
                        return ::core::result::Result::Ok(());
                    };
                    let prototype = if let ::core::option::Option::Some(nf) =
                        ctor.as_native_function()
                    {
                        nf.own_property_descriptor(heap, "prototype")
                            .ok()
                            .flatten()
                            .and_then(|d| match d.kind {
                                ::otter_vm::object::DescriptorKind::Data { value } => {
                                    value.as_object()
                                }
                                _ => ::core::option::Option::None,
                            })
                    } else if let ::core::option::Option::Some(obj) = ctor.as_object() {
                        ::otter_vm::object::get(obj, heap, "prototype")
                            .and_then(|v| v.as_object())
                    } else {
                        ::core::option::Option::None
                    };
                    let ::core::option::Option::Some(prototype) = prototype else {
                        return ::core::result::Result::Ok(());
                    };
                    let tag_sym =
                        well_known.get(::otter_vm::symbol::WellKnown::ToStringTag);
                    let value = ::otter_vm::string::JsString::from_str(#tag, heap)
                        .map_err(|_| ::otter_vm::JsSurfaceError::OutOfMemory)?;
                    ::otter_vm::object::define_own_symbol_property_partial(
                        prototype,
                        heap,
                        tag_sym,
                        ::otter_vm::object::PartialPropertyDescriptor {
                            value: ::core::option::Option::Some(
                                ::otter_vm::Value::string(value),
                            ),
                            writable: ::core::option::Option::Some(false),
                            enumerable: ::core::option::Option::Some(false),
                            configurable: ::core::option::Option::Some(true),
                            ..::core::default::Default::default()
                        },
                    );
                    ::core::result::Result::Ok(())
                }
            }
        }
        None => quote! {},
    };

    let bind_call = match install_on {
        Some(path) => quote! {
            // §<chapter> — nested constructor lives on a host
            // namespace object. The host resolver returns the
            // parent object; bind the constructor as an own data
            // property (writable / non-enumerable / configurable,
            // matching the conventional builtin shape).
            let host = #path(global, heap);
            let bind_desc = ::otter_vm::object::PropertyDescriptor::data(
                ctor_value,
                true,
                false,
                true,
            );
            if !::otter_vm::object::define_own_property(
                host,
                heap,
                <Self as ::otter_vm::intrinsic_install::BuiltinIntrinsic>::NAME,
                bind_desc,
            ) {
                return ::core::result::Result::Err(
                    ::otter_vm::JsSurfaceError::DefinePropertyFailed(
                        <Self as ::otter_vm::intrinsic_install::BuiltinIntrinsic>::NAME,
                    ),
                );
            }
        },
        None => quote! {
            ::otter_vm::bootstrap::define_global_value(
                global,
                heap,
                <Self as ::otter_vm::intrinsic_install::BuiltinIntrinsic>::NAME,
                ctor_value,
            );
        },
    };

    quote! {
        #[allow(non_upper_case_globals)]
        static #statics_ident: &[::otter_vm::MethodSpec] = &[
            #(#static_entries),*
        ];

        #[allow(non_upper_case_globals)]
        static #static_constants_ident: &[::otter_vm::ConstSpec] = &[
            #(#static_constant_entries),*
        ];

        #[allow(non_upper_case_globals)]
        static #prototype_constants_ident: &[::otter_vm::ConstSpec] = &[
            #(#prototype_constant_entries),*
        ];

        #[allow(non_upper_case_globals)]
        static #prototype_methods_ident: &[::otter_vm::MethodSpec] = &[
            #(#prototype_method_entries),*
        ];

        #[allow(non_upper_case_globals)]
        static #prototype_accessors_ident: &[::otter_vm::AccessorSpec] = &[
            #(#prototype_accessor_entries),*
        ];

        #[doc = "Generated constructor spec (see `couch!`)."]
        #[allow(non_upper_case_globals)]
        pub static #spec_ident: ::otter_vm::ConstructorSpec = ::otter_vm::ConstructorSpec {
            name: #name,
            length: #ctor_length,
            call: ::otter_vm::NativeCall::Static(#ctor_call),
            static_methods: #statics_ident,
            prototype_methods: #prototype_methods_ident,
            attrs: ::otter_vm::Attr::global_binding(),
        };

        #[doc = "Generated `BuiltinIntrinsic` adapter (see `couch!`)."]
        pub struct #intrinsic_ident;

        impl ::otter_vm::intrinsic_install::BuiltinIntrinsic for #intrinsic_ident {
            const NAME: &'static str = #name;
            const FEATURE: ::otter_vm::bootstrap::BootstrapFeatures = #feature_path;

            fn install(
                heap: &mut ::otter_gc::GcHeap,
                global: ::otter_vm::JsObject,
            ) -> ::core::result::Result<(), ::otter_vm::JsSurfaceError> {
                let global_root = ::otter_vm::Value::object(global);

                // Generated specs only ever carry `NativeCall::Static`;
                // every other variant is unreachable inside macro
                // expansion. Pattern out explicitly to keep
                // `cargo clippy --deny warnings` happy.
                let ctor_call = match #spec_ident.call {
                    ::otter_vm::NativeCall::Static(f) => f,
                    ::otter_vm::NativeCall::VmIntrinsic(_)
                    | ::otter_vm::NativeCall::Dynamic(_) => {
                        return ::core::result::Result::Err(
                            ::otter_vm::JsSurfaceError::DefinePropertyFailed(
                                "couch!: non-Static NativeCall in constructor spec",
                            ),
                        );
                    }
                };
                let ctor = #ctor_alloc_path(
                    heap,
                    #spec_ident.name,
                    #spec_ident.length,
                    ctor_call,
                    &[&global_root],
                )
                .map_err(::otter_vm::JsSurfaceError::from)?;
                // `ctor_value` is the single source of truth for the (possibly
                // relocated) constructor: it is threaded into every allocation
                // below, so the collector keeps it current, and each raw-handle
                // use re-resolves through it rather than reading a stale offset.
                let ctor_value = ::otter_vm::Value::native_function(ctor);
                #ctor_parent_link

                for method_spec in #spec_ident.static_methods.iter() {
                    let call_target = match method_spec.call {
                        ::otter_vm::NativeCall::Static(f) => f,
                        ::otter_vm::NativeCall::VmIntrinsic(_)
                        | ::otter_vm::NativeCall::Dynamic(_) => {
                            return ::core::result::Result::Err(
                                ::otter_vm::JsSurfaceError::DefinePropertyFailed(method_spec.name),
                            );
                        }
                    };
                    let fn_obj = ::otter_vm::bootstrap::native_static_with_value_roots(
                        heap,
                        method_spec.name,
                        method_spec.length,
                        call_target,
                        &[&global_root, &ctor_value],
                    )
                    .map_err(::otter_vm::JsSurfaceError::from)?;
                    let desc = ::otter_vm::object::PropertyDescriptor::data(
                        ::otter_vm::Value::native_function(fn_obj),
                        method_spec.attrs.writable,
                        method_spec.attrs.enumerable,
                        method_spec.attrs.configurable,
                    );
                    let ctor = ctor_value
                        .as_native_function()
                        .expect("couch!: constructor stays a native function across allocation");
                    if !ctor.define_own_property(heap, method_spec.name, desc) {
                        return ::core::result::Result::Err(
                            ::otter_vm::JsSurfaceError::DefinePropertyFailed(method_spec.name),
                        );
                    }
                }

                // Extra `static_method_specs = [path, ...]` paths —
                // iterate each pre-built `&[MethodSpec]` slice and
                // pin on the constructor as own data properties.
                // Mirrors the prototype.method_specs path so multi-row
                // surfaces (e.g. STRING_STATIC_METHODS) stay
                // declarative.
                #(#static_method_spec_iters)*

                // Static constants (e.g. `Number.NaN`, `Number.MAX_VALUE`).
                // Pinned as own data properties on the constructor
                // using the attrs supplied per row (defaults to
                // `Attr::read_only()` — non-writable / non-enumerable
                // / non-configurable, matching §21.1.2 etc.).
                for const_spec in #static_constants_ident.iter() {
                    let value = match const_spec.value {
                        ::otter_vm::ConstValue::Undefined => ::otter_vm::Value::undefined(),
                        ::otter_vm::ConstValue::Null => ::otter_vm::Value::null(),
                        ::otter_vm::ConstValue::Boolean(b) => ::otter_vm::Value::boolean(b),
                        ::otter_vm::ConstValue::Number(n) => ::otter_vm::Value::number_f64(n),
                    };
                    let desc = ::otter_vm::object::PropertyDescriptor::data(
                        value,
                        const_spec.attrs.writable,
                        const_spec.attrs.enumerable,
                        const_spec.attrs.configurable,
                    );
                    let ctor = ctor_value
                        .as_native_function()
                        .expect("couch!: constructor stays a native function across allocation");
                    if !ctor.define_own_property(heap, const_spec.name, desc) {
                        return ::core::result::Result::Err(
                            ::otter_vm::JsSurfaceError::DefinePropertyFailed(const_spec.name),
                        );
                    }
                }

                // §19.4 prototype object (only when the spec lists prototype
                // methods or accessors). Alloc empty prototype + link to
                // %Object.prototype% + pin each entry, then attach the
                // prototype back on the constructor as a non-writable /
                // non-enumerable / non-configurable own data property (matches
                // the canonical builtin prototype descriptor). Every install
                // allocation threads the rooted `global_root` / `ctor_value` /
                // `prototype_value` copies by reference, so the collector keeps
                // them current and no raw handle is read across an allocation.
                if #prototype_block_needed {
                    let prototype = ::otter_vm::bootstrap::alloc_object_with_value_roots_pub(
                        heap,
                        &[&global_root, &ctor_value],
                    )
                    .map_err(::otter_vm::JsSurfaceError::from)?;
                    #prototype_parent_link
                    let prototype_value = ::otter_vm::Value::object(prototype);

                    for method_spec in #spec_ident.prototype_methods.iter() {
                        let fn_obj = ::otter_vm::bootstrap::native_from_call_with_value_roots(
                            heap,
                            method_spec.name,
                            method_spec.length,
                            method_spec.call.clone(),
                            &[&global_root, &ctor_value, &prototype_value],
                        )
                        .map_err(::otter_vm::JsSurfaceError::from)?;
                        let desc = ::otter_vm::object::PropertyDescriptor::data(
                            ::otter_vm::Value::native_function(fn_obj),
                            method_spec.attrs.writable,
                            method_spec.attrs.enumerable,
                            method_spec.attrs.configurable,
                        );
                        let prototype = prototype_value
                            .as_object()
                            .expect("couch!: prototype stays an object across allocation");
                        if !::otter_vm::object::define_own_property(
                            prototype,
                            heap,
                            method_spec.name,
                            desc,
                        ) {
                            return ::core::result::Result::Err(
                                ::otter_vm::JsSurfaceError::DefinePropertyFailed(method_spec.name),
                            );
                        }
                    }

                    for accessor_spec in #prototype_accessors_ident.iter() {
                        let getter = match &accessor_spec.get {
                            ::core::option::Option::Some(call) => {
                                ::core::option::Option::Some(::otter_vm::Value::native_function(
                                    ::otter_vm::bootstrap::native_from_call_with_value_roots(
                                        heap,
                                        accessor_spec.get_name,
                                        0,
                                        call.clone(),
                                        &[&global_root, &ctor_value, &prototype_value],
                                    )
                                    .map_err(::otter_vm::JsSurfaceError::from)?,
                                ))
                            }
                            ::core::option::Option::None => ::core::option::Option::None,
                        };
                        // The getter is live across the setter allocation; thread
                        // it as an extra root so `getter_root` tracks any move.
                        let getter_root = getter.unwrap_or_else(::otter_vm::Value::undefined);
                        let setter = match &accessor_spec.set {
                            ::core::option::Option::Some(call) => {
                                ::core::option::Option::Some(::otter_vm::Value::native_function(
                                    ::otter_vm::bootstrap::native_from_call_with_value_roots(
                                        heap,
                                        accessor_spec.set_name,
                                        1,
                                        call.clone(),
                                        &[
                                            &global_root,
                                            &ctor_value,
                                            &prototype_value,
                                            &getter_root,
                                        ],
                                    )
                                    .map_err(::otter_vm::JsSurfaceError::from)?,
                                ))
                            }
                            ::core::option::Option::None => ::core::option::Option::None,
                        };
                        let getter = getter.map(|_| getter_root);
                        let descriptor = ::otter_vm::object::PropertyDescriptor::accessor(
                            getter,
                            setter,
                            accessor_spec.attrs.enumerable,
                            accessor_spec.attrs.configurable,
                        );
                        let prototype = prototype_value
                            .as_object()
                            .expect("couch!: prototype stays an object across allocation");
                        if !::otter_vm::object::define_own_property(
                            prototype,
                            heap,
                            accessor_spec.name,
                            descriptor,
                        ) {
                            return ::core::result::Result::Err(
                                ::otter_vm::JsSurfaceError::DefinePropertyFailed(
                                    accessor_spec.name,
                                ),
                            );
                        }
                    }

                    // Extra `method_specs = [path, ...]` paths — install each
                    // pre-built `&[MethodSpec]` slice through the same
                    // by-reference loop. Used by builtins (e.g. `Date`) whose
                    // prototype method list is generated by a separate
                    // declarative macro that produces a static slice.
                    #(#extra_method_spec_iters)*

                    // Prototype constants (e.g. per-kind
                    // `TypedArray.prototype.BYTES_PER_ELEMENT`).
                    for const_spec in #prototype_constants_ident.iter() {
                        let value = match const_spec.value {
                            ::otter_vm::ConstValue::Undefined => ::otter_vm::Value::undefined(),
                            ::otter_vm::ConstValue::Null => ::otter_vm::Value::null(),
                            ::otter_vm::ConstValue::Boolean(b) => ::otter_vm::Value::boolean(b),
                            ::otter_vm::ConstValue::Number(n) => ::otter_vm::Value::number_f64(n),
                        };
                        let desc = ::otter_vm::object::PropertyDescriptor::data(
                            value,
                            const_spec.attrs.writable,
                            const_spec.attrs.enumerable,
                            const_spec.attrs.configurable,
                        );
                        let prototype = prototype_value
                            .as_object()
                            .expect("couch!: prototype stays an object across allocation");
                        if !::otter_vm::object::define_own_property(
                            prototype,
                            heap,
                            const_spec.name,
                            desc,
                        ) {
                            return ::core::result::Result::Err(
                                ::otter_vm::JsSurfaceError::DefinePropertyFailed(const_spec.name),
                            );
                        }
                    }
                    let proto_desc = ::otter_vm::object::PropertyDescriptor::data(
                        prototype_value,
                        false,
                        false,
                        false,
                    );
                    let ctor = ctor_value
                        .as_native_function()
                        .expect("couch!: constructor stays a native function across allocation");
                    if !ctor.define_own_property(heap, "prototype", proto_desc) {
                        return ::core::result::Result::Err(
                            ::otter_vm::JsSurfaceError::DefinePropertyFailed("prototype"),
                        );
                    }
                    // §19.4 — every builtin constructor whose
                    // prototype carries methods/accessors gets a
                    // `prototype.constructor = ctor` back-pointer
                    // (writable / non-enumerable / configurable per
                    // spec).
                    let ctor_back_desc = ::otter_vm::object::PropertyDescriptor::data(
                        ctor_value,
                        true,
                        false,
                        true,
                    );
                    let prototype = prototype_value
                        .as_object()
                        .expect("couch!: prototype stays an object across allocation");
                    let _ = ::otter_vm::object::define_own_property(
                        prototype,
                        heap,
                        "constructor",
                        ctor_back_desc,
                    );
                }

                #bind_call
                #post_install_call
                ::core::result::Result::Ok(())
            }

            #install_well_knowns_fn
        }
    }
    .into()
}
