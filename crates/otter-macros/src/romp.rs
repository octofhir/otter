//! `romp!` proc macro — extension bundle declaration.
//!
//! A romp of otters: one declaration bundles an extension's native
//! classes and its JS half into a static `Extension` descriptor the
//! runtime installs as a unit. Source bytes and defined names share the
//! product build declaration; no runtime name registry is introduced.
//!
//! # Contents
//! - Native class declarations and one optional build-produced JS bundle.
//! - Static `Extension` emission; compilation belongs to the product build script.
//!
//! # Invariants
//! Classes install eagerly in declaration order. A nonempty JS half is one
//! source/code/defines owner, executed after native installers without splitting
//! its top-level scope. Runtime compile hooks receive that exact source.
//!
//! # Surface
//!
//! ```rust,ignore
//! romp! {
//!     name = "web",
//!     classes = [url::WebUrlIntrinsic, blob::BlobIntrinsic],
//!     js = Some(include!(concat!(env!("OUT_DIR"), "/web-bootstrap.rs"))),
//! }
//! ```
//!
//! The generated `pub static <NAME>_EXTENSION` uses the runtime-owned static
//! descriptor. Override its name with `ident = MY_EXT`. Native-only bundles use
//! `js = None`. Defined names come from the same product build declaration as
//! the source bundle; there is no lazy-global or parallel name registry.
//!
//! # See also
//! - [`crate::js_class`](super::js_class) for class declarations.
//! - `docs/site/src/content/docs/extensions/declarative-bindings.md`

use proc_macro::TokenStream;
use proc_macro2::Span;
use quote::{format_ident, quote};
use syn::parse::{Parse, ParseStream};
use syn::{Error, Expr, Ident, LitStr, Path, Result, Token, bracketed};

struct RompInput {
    name: LitStr,
    ident: Option<Ident>,
    classes: Vec<Path>,
    js: Expr,
}

impl Parse for RompInput {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let mut name: Option<LitStr> = None;
        let mut ident: Option<Ident> = None;
        let mut classes = Vec::new();
        let mut js = syn::parse_quote!(None);
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
                "js" => js = input.parse()?,
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
    let js = &input.js;
    quote! {
        #[doc = "Generated extension descriptor (see `romp!`)."]
        pub static #static_ident: ::otter_vm::__macro_support::Extension = ::otter_vm::__macro_support::Extension {
            name: #name,
            classes: &[
                #(::otter_vm::__macro_support::GlobalClass::from_intrinsic::<#classes>(),)*
            ],
            js: #js,
        };
    }
    .into()
}
