//! `#[derive(IntoJs)]` — struct-to-object return construction.
//!
//! For a named-field struct, emits `marshal::IntoJs` building a plain
//! object (`%Object.prototype%`) with one data property per field, in
//! declaration order. Field ident is the JS property name verbatim
//! (never case-converted); override with `#[js(name = "…")]`.
//!
//! ```rust,ignore
//! #[derive(IntoJs)]
//! pub struct Row {
//!     id: f64,
//!     #[js(name = "displayName")]
//!     display_name: String,
//! }
//! ```
//!
//! # See also
//! - [`crate::derive_from_js`](super::derive_from_js) — the
//!   extraction direction.

use proc_macro::TokenStream;
use quote::quote;
use syn::spanned::Spanned;
use syn::{Data, DeriveInput, Error, Fields, LitStr, Result};

/// Expand `#[derive(IntoJs)]`.
pub(crate) fn expand(input: TokenStream) -> TokenStream {
    let input = syn::parse_macro_input!(input as DeriveInput);
    match expand_inner(&input) {
        Ok(tokens) => tokens.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

fn expand_inner(input: &DeriveInput) -> Result<proc_macro2::TokenStream> {
    let Data::Struct(data) = &input.data else {
        return Err(Error::new(input.span(), "IntoJs derives on structs"));
    };
    if !input.generics.params.is_empty() {
        return Err(Error::new(
            input.generics.span(),
            "IntoJs does not support generics",
        ));
    }
    let Fields::Named(fields) = &data.fields else {
        return Err(Error::new(
            data.fields.span(),
            "IntoJs requires named fields",
        ));
    };

    let mut writes = Vec::new();
    for field in &fields.named {
        let field_ident = field
            .ident
            .as_ref()
            .ok_or_else(|| Error::new(field.span(), "IntoJs fields must be named"))?;
        let mut js_name = field_ident.to_string();
        for attr in &field.attrs {
            if !attr.path().is_ident("js") {
                continue;
            }
            attr.parse_nested_meta(|meta| {
                if meta.path.is_ident("name") {
                    let lit: LitStr = meta.value()?.parse()?;
                    js_name = lit.value();
                    Ok(())
                } else {
                    Err(meta.error("js field options: `name = \"…\"`"))
                }
            })?;
        }
        let js_name = LitStr::new(&js_name, field_ident.span());
        writes.push(quote! {
            let __value = ::otter_vm::marshal::IntoJs::into_js(self.#field_ident, cx)?;
            cx.set(__object, #js_name, __value)?;
        });
    }

    let ident = &input.ident;
    Ok(quote! {
        impl ::otter_vm::marshal::IntoJs for #ident {
            fn into_js<'s>(
                self,
                cx: &mut ::otter_vm::marshal::MarshalCx<'_, '_, 's>,
            ) -> ::core::result::Result<
                ::otter_vm::marshal::JsValue<'s>,
                ::otter_vm::marshal::JsError,
            > {
                let __object = cx.object()?;
                #(#writes)*
                ::core::result::Result::Ok(__object)
            }
        }
    })
}
