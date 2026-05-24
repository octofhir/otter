//! `holt!` proc macro — namespace intrinsic generator.
//!
//! See the crate-level docs and
//! [`docs/book/src/macros/design.md`](../../../docs/book/src/macros/design.md)
//! for the naming theme and the full surface. This module owns the
//! parsing + expansion for the `holt!` invocation only.
//!
//! # Contents
//! - [`expand`] — entry called from the proc-macro shim in
//!   [`crate::holt`](super) re-exported as `holt!`.
//! - [`HoltInput`] — parsed top-level fields.
//! - [`MethodEntry`] — single method-table row
//!   (`"name" / length => path,`).
//!
//! # Invariants
//! - Method names are unique per `holt!`; duplicates surface as a
//!   compile error pointing at the offending literal.
//! - Generated code references `::otter_vm::*` and
//!   `::otter_gc::GcHeap` only; nothing from `otter-macros`.
//! - Output type idents:
//!   - `spec`: derived as `<NAME>_SPEC` (name uppercased + `_SPEC`)
//!     unless the caller passes `spec = OVERRIDE_IDENT,`.
//!   - `intrinsic`: derived as `Intrinsic` unless the caller passes
//!     `intrinsic = OverrideIdent,`. The default matches the
//!     hand-written `crates/otter-vm/src/intrinsics/<name>.rs`
//!     convention where each per-intrinsic module exposes one
//!     `pub struct Intrinsic;`.
//!
//! # See also
//! - [`crate::raft`](super::raft) — grouped method spec used as the
//!   table form of a `holt!` body.
//! - [`docs/book/src/macros/design.md`](../../../docs/book/src/macros/design.md)

use proc_macro::TokenStream;
use proc_macro2::Span;
use quote::{format_ident, quote};
use std::collections::BTreeSet;
use syn::parse::{Parse, ParseStream};
use syn::{
    Expr, Ident, LitBool, LitInt, LitStr, Path, Result, Token, braced, bracketed, parenthesized,
    parse_macro_input,
};

/// Single `"name" / length => path [attrs = <ident>]` row of a
/// `holt!` / `couch!` method table.
pub(crate) struct MethodEntry {
    pub(crate) js_name: LitStr,
    pub(crate) length: u8,
    pub(crate) call: Path,
    pub(crate) attrs: Option<Ident>,
}

impl Parse for MethodEntry {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let js_name: LitStr = input.parse()?;
        input.parse::<Token![/]>()?;
        let length_lit: LitInt = input.parse()?;
        input.parse::<Token![=>]>()?;
        let call: Path = input.parse()?;
        let attrs = if input.peek(syn::Ident) {
            // Optional `attrs = <factory_ident>` suffix.
            let key: Ident = input.parse()?;
            if key != "attrs" {
                return Err(syn::Error::new(
                    key.span(),
                    format!(
                        "expected `attrs = <Attr factory ident>` or a comma after the method \
                         entry; got `{key}`"
                    ),
                ));
            }
            input.parse::<Token![=]>()?;
            Some(input.parse()?)
        } else {
            None
        };
        Ok(Self {
            js_name,
            length: length_lit.base10_parse()?,
            call,
            attrs,
        })
    }
}

/// Single `("name", get = getter, set = setter, attrs)` row of a
/// `holt!` / `couch!` accessor table. Either `get` or `set` may be
/// omitted (one-sided accessor); `attrs` defaults to
/// `builtin_function`.
pub(crate) struct AccessorEntry {
    pub(crate) js_name: LitStr,
    pub(crate) get: Option<Path>,
    pub(crate) set: Option<Path>,
    pub(crate) attrs: Option<Ident>,
}

impl Parse for AccessorEntry {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let inner;
        parenthesized!(inner in input);
        let js_name: LitStr = inner.parse()?;
        let mut get: Option<Path> = None;
        let mut set: Option<Path> = None;
        let mut attrs: Option<Ident> = None;
        while !inner.is_empty() {
            inner.parse::<Token![,]>()?;
            if inner.is_empty() {
                break;
            }
            let key: Ident = inner.parse()?;
            if key == "attrs" {
                // Plain `attrs = <ident>` form (default factory).
                inner.parse::<Token![=]>()?;
                attrs = Some(inner.parse()?);
                continue;
            }
            inner.parse::<Token![=]>()?;
            match key.to_string().as_str() {
                "get" => get = Some(inner.parse()?),
                "set" => set = Some(inner.parse()?),
                other => {
                    return Err(syn::Error::new(
                        key.span(),
                        format!(
                            "unknown accessor field `{other}` — expected `get`, `set`, or `attrs`"
                        ),
                    ));
                }
            }
        }
        if get.is_none() && set.is_none() {
            return Err(syn::Error::new_spanned(
                &js_name,
                format!(
                    "accessor `{}` declares neither `get =` nor `set =`",
                    js_name.value()
                ),
            ));
        }
        Ok(Self {
            js_name,
            get,
            set,
            attrs,
        })
    }
}

/// Single constant entry inside `constants = [...]`.
///
/// Syntax: `("NAME", Kind(expr), attrs)` where `Kind` is one of
/// `Undefined`, `Null`, `Boolean`, `Number`. `attrs` is one of the
/// `Attr` factory shortcuts: `read_only`, `data`, `builtin_function`,
/// `global_binding`, defaulting to `read_only` when omitted.
pub(crate) struct ConstantEntry {
    pub(crate) js_name: LitStr,
    pub(crate) kind: Ident,
    pub(crate) value: Option<Expr>,
    pub(crate) attrs: Option<Ident>,
}

impl Parse for ConstantEntry {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let inner;
        parenthesized!(inner in input);
        let js_name: LitStr = inner.parse()?;
        inner.parse::<Token![,]>()?;
        // `Kind(expr)` or `Kind` for nullary variants (Undefined / Null).
        let kind: Ident = inner.parse()?;
        let value = if inner.peek(syn::token::Paren) {
            let value_body;
            parenthesized!(value_body in inner);
            let expr: Expr = value_body.parse()?;
            Some(expr)
        } else {
            None
        };
        let attrs = if inner.peek(Token![,]) {
            inner.parse::<Token![,]>()?;
            let attr_ident: Ident = inner.parse()?;
            Some(attr_ident)
        } else {
            None
        };
        Ok(Self {
            js_name,
            kind,
            value,
            attrs,
        })
    }
}

/// Parsed body of a `holt!` invocation.
pub(crate) struct HoltInput {
    pub(crate) name: LitStr,
    pub(crate) feature: Ident,
    pub(crate) spec_ident: Ident,
    pub(crate) intrinsic_ident: Ident,
    pub(crate) methods: Vec<MethodEntry>,
    pub(crate) constants: Vec<ConstantEntry>,
    pub(crate) accessors: Vec<AccessorEntry>,
    /// When `true`, the generated `install` body links the
    /// namespace's `[[Prototype]]` to `%Object.prototype%`
    /// (looked up through `Object.prototype` on the global
    /// passed to `install`). Defaults to `false` to match the
    /// historical hand-written installers for Math / JSON /
    /// Console, which omitted the link.
    pub(crate) link_object_prototype: bool,
}

impl Parse for HoltInput {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let mut name: Option<LitStr> = None;
        let mut feature: Option<Ident> = None;
        let mut spec_override: Option<Ident> = None;
        let mut intrinsic_override: Option<Ident> = None;
        let mut methods: Vec<MethodEntry> = Vec::new();
        let mut constants: Vec<ConstantEntry> = Vec::new();
        let mut accessors: Vec<AccessorEntry> = Vec::new();
        let mut link_object_prototype = false;
        let mut methods_seen = false;

        while !input.is_empty() {
            let key: Ident = input.parse()?;
            input.parse::<Token![=]>()?;
            match key.to_string().as_str() {
                "name" => name = Some(input.parse()?),
                "feature" => feature = Some(input.parse()?),
                "spec" => spec_override = Some(input.parse()?),
                "intrinsic" => intrinsic_override = Some(input.parse()?),
                "methods" => {
                    methods_seen = true;
                    let body;
                    braced!(body in input);
                    while !body.is_empty() {
                        methods.push(body.parse()?);
                        if body.peek(Token![,]) {
                            body.parse::<Token![,]>()?;
                        }
                    }
                }
                "constants" => {
                    let body;
                    bracketed!(body in input);
                    while !body.is_empty() {
                        constants.push(body.parse()?);
                        if body.peek(Token![,]) {
                            body.parse::<Token![,]>()?;
                        }
                    }
                }
                "accessors" => {
                    let body;
                    bracketed!(body in input);
                    while !body.is_empty() {
                        accessors.push(body.parse()?);
                        if body.peek(Token![,]) {
                            body.parse::<Token![,]>()?;
                        }
                    }
                }
                "link_object_prototype" => {
                    let lit: LitBool = input.parse()?;
                    link_object_prototype = lit.value;
                }
                other => {
                    return Err(syn::Error::new(
                        key.span(),
                        format!(
                            "unknown `holt!` field `{other}` — expected `name`, `feature`, \
                             `spec`, `intrinsic`, `methods`, `constants`, `accessors`, \
                             or `link_object_prototype`"
                        ),
                    ));
                }
            }
            if input.peek(Token![,]) {
                input.parse::<Token![,]>()?;
            }
        }

        let name = name
            .ok_or_else(|| syn::Error::new(Span::call_site(), "holt!: missing `name = \"...\"`"))?;
        let feature = feature.ok_or_else(|| {
            syn::Error::new(
                Span::call_site(),
                "holt!: missing `feature = <BootstrapFeatures variant>` (e.g. `feature = CORE`)",
            )
        })?;
        if !methods_seen {
            return Err(syn::Error::new(
                Span::call_site(),
                "holt!: missing `methods = { ... }` block (use an empty `{}` for namespaces with no methods)",
            ));
        }

        let spec_ident = spec_override.unwrap_or_else(|| {
            let upper = name.value().to_ascii_uppercase();
            // Replace non-identifier chars with `_` so `name = "%TypedArray%"`
            // produces a usable Rust ident.
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
            methods,
            constants,
            accessors,
            link_object_prototype,
        })
    }
}

/// Map an `attrs = <ident>` shortcut to the matching `Attr::*()`
/// factory path. Unknown idents fall back to `default_factory`
/// silently — the syn::Error path is awkward inside the closure
/// chain so we defer to the rust type checker (`Attr::<unknown>`
/// compile error) for diagnostics if the ident is malformed.
///
/// `pub(crate)` so the [`crate::couch`] module can reuse the same
/// resolution for prototype methods + accessors.
pub(crate) fn attrs_factory_path(
    attrs: Option<&Ident>,
    default_factory: &str,
) -> proc_macro2::TokenStream {
    let factory = attrs
        .map(|ident| ident.to_string())
        .unwrap_or_else(|| default_factory.to_string());
    let factory_ident = Ident::new(&factory, Span::call_site());
    quote! { ::otter_vm::Attr::#factory_ident() }
}

pub(crate) fn expand(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as HoltInput);
    let HoltInput {
        name,
        feature,
        spec_ident,
        intrinsic_ident,
        methods,
        constants,
        accessors,
        link_object_prototype,
    } = input;

    // Duplicate-name guards.
    let mut seen = BTreeSet::new();
    for method in &methods {
        if !seen.insert(method.js_name.value()) {
            return syn::Error::new_spanned(
                &method.js_name,
                format!("holt!: duplicate method name `{}`", method.js_name.value()),
            )
            .to_compile_error()
            .into();
        }
    }
    let mut seen_const = BTreeSet::new();
    for c in &constants {
        if !seen_const.insert(c.js_name.value()) {
            return syn::Error::new_spanned(
                &c.js_name,
                format!("holt!: duplicate constant name `{}`", c.js_name.value()),
            )
            .to_compile_error()
            .into();
        }
    }
    let mut seen_accessor = BTreeSet::new();
    for a in &accessors {
        if !seen_accessor.insert(a.js_name.value()) {
            return syn::Error::new_spanned(
                &a.js_name,
                format!("holt!: duplicate accessor name `{}`", a.js_name.value()),
            )
            .to_compile_error()
            .into();
        }
    }

    let methods_ident = format_ident!("__OTTER_{}_METHODS", spec_ident);
    let constants_ident = format_ident!("__OTTER_{}_CONSTANTS", spec_ident);
    let accessors_ident = format_ident!("__OTTER_{}_ACCESSORS", spec_ident);
    let method_entries = methods.iter().map(|m| {
        let js_name = &m.js_name;
        let length = m.length;
        let call = &m.call;
        let attrs_path = attrs_factory_path(m.attrs.as_ref(), "builtin_function");
        quote! {
            ::otter_vm::MethodSpec {
                name: #js_name,
                length: #length,
                attrs: #attrs_path,
                call: ::otter_vm::NativeCall::Static(#call),
            }
        }
    });

    let accessor_entries = accessors.iter().map(|a| {
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
        let attrs_path = attrs_factory_path(a.attrs.as_ref(), "builtin_function");
        quote! {
            ::otter_vm::AccessorSpec {
                name: #js_name,
                get: #get_tokens,
                set: #set_tokens,
                attrs: #attrs_path,
            }
        }
    });

    let constant_entries = constants.iter().map(|c| {
        let js_name = &c.js_name;
        let attrs_path = attrs_factory_path(c.attrs.as_ref(), "read_only");
        let value_tokens = match (c.kind.to_string().as_str(), c.value.as_ref()) {
            ("Undefined", None) => quote! { ::otter_vm::ConstValue::Undefined },
            ("Null", None) => quote! { ::otter_vm::ConstValue::Null },
            ("Boolean", Some(expr)) => quote! { ::otter_vm::ConstValue::Boolean(#expr) },
            ("Number", Some(expr)) => quote! { ::otter_vm::ConstValue::Number(#expr) },
            ("Undefined" | "Null", Some(_)) => {
                return syn::Error::new_spanned(
                    &c.kind,
                    format!(
                        "holt!: `{}` constant takes no value — drop the `(expr)` suffix",
                        c.kind
                    ),
                )
                .to_compile_error();
            }
            ("Boolean" | "Number", None) => {
                return syn::Error::new_spanned(
                    &c.kind,
                    format!("holt!: `{}` constant requires a `(expr)` value", c.kind),
                )
                .to_compile_error();
            }
            (other, _) => {
                return syn::Error::new_spanned(
                    &c.kind,
                    format!(
                        "holt!: unknown constant kind `{other}` — expected one of \
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

    // `feature` is the unqualified variant name; emit the qualified
    // path against `crate::bootstrap::BootstrapFeatures`. The macro
    // intentionally requires the variant ident bare (e.g. `CORE`)
    // for readability — `feature = BootstrapFeatures::CORE` would
    // be redundant in every invocation.
    let feature_path = quote! { ::otter_vm::bootstrap::BootstrapFeatures::#feature };

    quote! {
        #[allow(non_upper_case_globals)]
        static #methods_ident: &[::otter_vm::MethodSpec] = &[
            #(#method_entries),*
        ];

        #[allow(non_upper_case_globals)]
        static #constants_ident: &[::otter_vm::ConstSpec] = &[
            #(#constant_entries),*
        ];

        #[allow(non_upper_case_globals)]
        static #accessors_ident: &[::otter_vm::AccessorSpec] = &[
            #(#accessor_entries),*
        ];

        #[doc = "Generated namespace spec (see `holt!`)."]
        #[allow(non_upper_case_globals)]
        pub static #spec_ident: ::otter_vm::NamespaceSpec = ::otter_vm::NamespaceSpec {
            name: #name,
            methods: #methods_ident,
            accessors: #accessors_ident,
            constants: #constants_ident,
            attrs: ::otter_vm::Attr::global_binding(),
        };

        #[doc = "Generated `BuiltinIntrinsic` adapter (see `holt!`)."]
        pub struct #intrinsic_ident;

        impl ::otter_vm::intrinsic_install::BuiltinIntrinsic for #intrinsic_ident {
            const NAME: &'static str = #name;
            const FEATURE: ::otter_vm::bootstrap::BootstrapFeatures = #feature_path;

            fn install(
                heap: &mut ::otter_gc::GcHeap,
                global: ::otter_vm::JsObject,
            ) -> ::core::result::Result<(), ::otter_vm::JsSurfaceError> {
                let global_root = ::otter_vm::Value::object(global);
                let namespace = ::otter_vm::NamespaceBuilder::from_spec_with_value_roots(
                    heap,
                    &#spec_ident,
                    ::std::vec![global_root],
                )
                .map_err(::otter_vm::JsSurfaceError::from)?
                .build()?;
                // §28.1 / §25.4 link to %Object.prototype% when the
                // caller opts in via `link_object_prototype = true`.
                // Built-in object spec calls for every ordinary
                // namespace to inherit from `%Object.prototype%`;
                // current hand-written installers for Math / JSON /
                // Console skip it (their JS surface still works
                // because property lookup falls through to the
                // empty `[[Prototype]]` chain). Defaulting the
                // flag to `false` preserves the existing per-port
                // shape; Reflect and Atomics opt in.
                if #link_object_prototype
                    && let ::core::option::Option::Some(object_ctor) =
                        ::otter_vm::object::get(global, heap, "Object")
                            .and_then(|v| v.as_object())
                    && let ::core::option::Option::Some(object_proto) =
                        ::otter_vm::object::get(object_ctor, heap, "prototype")
                            .and_then(|v| v.as_object())
                {
                    ::otter_vm::object::set_prototype(
                        namespace,
                        heap,
                        ::core::option::Option::Some(object_proto),
                    );
                }
                ::otter_vm::bootstrap::define_global_value(
                    global,
                    heap,
                    <Self as ::otter_vm::intrinsic_install::BuiltinIntrinsic>::NAME,
                    ::otter_vm::Value::object(namespace),
                );
                ::core::result::Result::Ok(())
            }
        }
    }
    .into()
}
