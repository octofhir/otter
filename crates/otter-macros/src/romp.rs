//! `romp!` proc macro — extension bundle declaration.
//!
//! A romp of otters: one declaration bundles an extension's native
//! classes and its JS half into a static `Extension` descriptor the
//! runtime installs as a unit. The name registry is derived from the
//! declaration — no hand-maintained global list, no install
//! choreography.
//!
//! # Surface
//!
//! ```rust,ignore
//! romp! {
//!     name = "web",
//!     classes = [url::WebUrlIntrinsic, blob::BlobIntrinsic, blob::FileIntrinsic],
//!     js = [
//!         (include_str!("web_bootstrap.js"), defines = ["Event", "EventTarget", /* … */]),
//!         (include_str!("web_streams.js"), defines = ["ReadableStream", /* … */]),
//!     ],
//! }
//! ```
//!
//! Generated symbols:
//! - `pub static <NAME>_EXTENSION: ::otter_vm::__macro_support::Extension` (upper-cased
//!   `name`; override with `ident = MY_EXT`).
//!
//! Semantics carried by the runtime (`RuntimeBuilder::extension`):
//! classes install eagerly in declaration order (subclasses list
//! their parent first); every `js` source registers under one native
//! lazy-global group keyed by the union of its `defines`, evaluated
//! once on first touch of any of those names, in declaration order.
//!
//! `defines` is explicit because a proc macro cannot parse the JS
//! source; keep it honest with a def-scan test comparing
//! `Extension::lazy_names()` against the sources (see
//! `otter-web`'s `lazy_global_names_match_shim_def_calls`).
//!
//! # See also
//! - [`crate::js_class`](super::js_class) — the class declarations
//!   listed in `classes`.
//! - `EXTENSION_API_PLAN.md` §5 — the design.

use proc_macro::TokenStream;
use proc_macro2::Span;
use quote::{format_ident, quote};
use syn::parse::{Parse, ParseStream};
use syn::{Error, Expr, Ident, LitStr, Path, Result, Token, bracketed, parenthesized};

struct JsEntry {
    source: Expr,
    defines: Vec<LitStr>,
}

impl Parse for JsEntry {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let inner;
        parenthesized!(inner in input);
        let source: Expr = inner.parse()?;
        inner.parse::<Token![,]>()?;
        let key: Ident = inner.parse()?;
        if key != "defines" {
            return Err(Error::new(key.span(), "expected `defines = [\"…\", …]`"));
        }
        inner.parse::<Token![=]>()?;
        let list;
        bracketed!(list in inner);
        let mut defines = Vec::new();
        while !list.is_empty() {
            defines.push(list.parse()?);
            if list.peek(Token![,]) {
                list.parse::<Token![,]>()?;
            }
        }
        if inner.peek(Token![,]) {
            inner.parse::<Token![,]>()?;
        }
        Ok(Self { source, defines })
    }
}

struct RompInput {
    name: LitStr,
    ident: Option<Ident>,
    classes: Vec<Path>,
    js: Vec<JsEntry>,
}

impl Parse for RompInput {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let mut name: Option<LitStr> = None;
        let mut ident: Option<Ident> = None;
        let mut classes = Vec::new();
        let mut js = Vec::new();
        while !input.is_empty() {
            let key: Ident = input.parse()?;
            input.parse::<Token![=]>()?;
            match key.to_string().as_str() {
                "name" => name = Some(input.parse()?),
                "ident" => ident = Some(input.parse()?),
                "classes" => {
                    let list;
                    bracketed!(list in input);
                    while !list.is_empty() {
                        classes.push(list.parse()?);
                        if list.peek(Token![,]) {
                            list.parse::<Token![,]>()?;
                        }
                    }
                }
                "js" => {
                    let list;
                    bracketed!(list in input);
                    while !list.is_empty() {
                        js.push(list.parse()?);
                        if list.peek(Token![,]) {
                            list.parse::<Token![,]>()?;
                        }
                    }
                }
                other => {
                    return Err(Error::new(
                        key.span(),
                        format!(
                            "unknown `romp!` field `{other}` — expected \
                             `name`, `ident`, `classes`, or `js`"
                        ),
                    ));
                }
            }
            if input.peek(Token![,]) {
                input.parse::<Token![,]>()?;
            }
        }
        Ok(Self {
            name: name
                .ok_or_else(|| Error::new(Span::call_site(), "romp! requires `name = \"…\"`"))?,
            ident,
            classes,
            js,
        })
    }
}

/// Expand `romp!` — see the module docs for the surface.
pub(crate) fn expand(input: TokenStream) -> TokenStream {
    let input = syn::parse_macro_input!(input as RompInput);
    let name = &input.name;
    let static_ident = input.ident.unwrap_or_else(|| {
        let upper: String = name
            .value()
            .to_ascii_uppercase()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect();
        format_ident!("{upper}_EXTENSION", span = name.span())
    });
    let classes = &input.classes;
    let js_rows = input.js.iter().map(|entry| {
        let source = &entry.source;
        let defines = &entry.defines;
        quote! {
            ::otter_vm::__macro_support::ExtensionJs {
                source: #source,
                defines: &[#(#defines),*],
            }
        }
    });
    quote! {
        #[doc = "Generated extension descriptor (see `romp!`)."]
        pub static #static_ident: ::otter_vm::__macro_support::Extension = ::otter_vm::__macro_support::Extension {
            name: #name,
            classes: &[
                #(::otter_vm::__macro_support::GlobalClass::from_intrinsic::<#classes>(),)*
            ],
            js: &[#(#js_rows),*],
        };
    }
    .into()
}
