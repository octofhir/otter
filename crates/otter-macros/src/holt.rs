//! `holt!` proc macro — namespace intrinsic generator.
//!
//! See the crate-level docs and
//! [`docs/otter-macros-design.md`](../../../docs/otter-macros-design.md)
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
//! - [`docs/otter-macros-design.md`](../../../docs/otter-macros-design.md)

use proc_macro::TokenStream;
use proc_macro2::Span;
use quote::{format_ident, quote};
use std::collections::BTreeSet;
use syn::parse::{Parse, ParseStream};
use syn::{Ident, LitInt, LitStr, Path, Result, Token, braced, parse_macro_input};

/// Single `"name" / length => path` row of a `holt!` method table.
pub(crate) struct MethodEntry {
    pub(crate) js_name: LitStr,
    pub(crate) length: u8,
    pub(crate) call: Path,
}

impl Parse for MethodEntry {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let js_name: LitStr = input.parse()?;
        input.parse::<Token![/]>()?;
        let length_lit: LitInt = input.parse()?;
        input.parse::<Token![=>]>()?;
        let call: Path = input.parse()?;
        Ok(Self {
            js_name,
            length: length_lit.base10_parse()?,
            call,
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
}

impl Parse for HoltInput {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let mut name: Option<LitStr> = None;
        let mut feature: Option<Ident> = None;
        let mut spec_override: Option<Ident> = None;
        let mut intrinsic_override: Option<Ident> = None;
        let mut methods: Vec<MethodEntry> = Vec::new();
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
                other => {
                    return Err(syn::Error::new(
                        key.span(),
                        format!(
                            "unknown `holt!` field `{other}` — expected `name`, `feature`, \
                             `spec`, `intrinsic`, or `methods`"
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
        })
    }
}

pub(crate) fn expand(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as HoltInput);
    let HoltInput {
        name,
        feature,
        spec_ident,
        intrinsic_ident,
        methods,
    } = input;

    // Duplicate-name guard.
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

    let methods_ident = format_ident!("__OTTER_{}_METHODS", spec_ident);
    let method_entries = methods.iter().map(|m| {
        let js_name = &m.js_name;
        let length = m.length;
        let call = &m.call;
        quote! {
            ::otter_vm::MethodSpec {
                name: #js_name,
                length: #length,
                attrs: ::otter_vm::Attr::builtin_function(),
                call: ::otter_vm::NativeCall::Static(#call),
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

        #[doc = "Generated namespace spec (see `holt!`)."]
        #[allow(non_upper_case_globals)]
        pub static #spec_ident: ::otter_vm::NamespaceSpec = ::otter_vm::NamespaceSpec {
            name: #name,
            methods: #methods_ident,
            accessors: &[],
            constants: &[],
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
