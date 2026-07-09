//! `#[derive(FromJs)]` — WebIDL dictionary and union extraction.
//!
//! Two shapes:
//!
//! **Dictionary** — a named-field struct. Each field reads the JS
//! member of the same name (override with `#[js(name = "…")]` — the
//! field ident is used verbatim, never case-converted). Members are
//! read in lexicographic JS-name order, matching WebIDL's observable
//! getter order. An `Option<T>` field is optional (`absent`/nullish →
//! `None`); a field marked `#[js(default)]` falls back to
//! `Default::default()`; any other field is required and its absence
//! is a `TypeError`. A nullish dictionary value converts to
//! all-defaults (an error if any member is required); any other
//! non-object is a `TypeError`.
//!
//! **Union** — an enum whose variants each hold one unnamed field.
//! Variants are probed in declaration order via
//! `marshal::JsUnionProbe` (type tests, never trial coercion — WebIDL
//! distinguishability); the final variant is the unconditional
//! catch-all, so coercing types (strings, numbers) must be declared
//! last.
//!
//! ```rust,ignore
//! #[derive(FromJs, Default)]
//! pub struct FilePropertyBag {
//!     #[js(name = "type", default)]
//!     content_type: USVString,
//!     #[js(name = "lastModified")]
//!     last_modified: Option<f64>,
//! }
//!
//! #[derive(FromJs)]
//! pub enum BlobPart {
//!     Blob(Blob),            // brand-probed
//!     Buffer(BufferSource),  // buffer-probed
//!     Text(USVString),       // catch-all — must be last
//! }
//! ```
//!
//! # See also
//! - [`crate::js_class`](super::js_class) — consumes these shapes in
//!   constructor/method signatures.

use proc_macro::TokenStream;
use quote::quote;
use syn::spanned::Spanned;
use syn::{Data, DeriveInput, Error, Field, Fields, Ident, LitStr, Result};

/// Per-field dictionary options parsed from `#[js(...)]`.
struct MemberOptions {
    js_name: String,
    default: bool,
}

fn member_options(field: &Field) -> Result<MemberOptions> {
    let ident = field
        .ident
        .as_ref()
        .ok_or_else(|| Error::new(field.span(), "dictionary fields must be named"))?;
    let mut js_name = ident.to_string();
    let mut default = false;
    for attr in &field.attrs {
        if !attr.path().is_ident("js") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("name") {
                let lit: LitStr = meta.value()?.parse()?;
                js_name = lit.value();
                Ok(())
            } else if meta.path.is_ident("default") {
                default = true;
                Ok(())
            } else {
                Err(meta.error("js field options are `name = \"…\"` and `default`"))
            }
        })?;
    }
    Ok(MemberOptions { js_name, default })
}

fn type_is_option(ty: &syn::Type) -> bool {
    let syn::Type::Path(path) = ty else {
        return false;
    };
    path.path
        .segments
        .last()
        .is_some_and(|segment| segment.ident == "Option")
}

/// Expand `#[derive(FromJs)]`.
pub(crate) fn expand(input: TokenStream) -> TokenStream {
    let input = syn::parse_macro_input!(input as DeriveInput);
    let expanded = match &input.data {
        Data::Struct(data) => expand_dictionary(&input, data),
        Data::Enum(data) => expand_union(&input, data),
        Data::Union(_) => Err(Error::new(
            input.span(),
            "FromJs derives on structs (dictionaries) and enums (unions)",
        )),
    };
    match expanded {
        Ok(tokens) => tokens.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

fn expand_dictionary(
    input: &DeriveInput,
    data: &syn::DataStruct,
) -> Result<proc_macro2::TokenStream> {
    if !input.generics.params.is_empty() {
        return Err(Error::new(
            input.generics.span(),
            "FromJs dictionaries do not support generics",
        ));
    }
    let Fields::Named(fields) = &data.fields else {
        return Err(Error::new(
            data.fields.span(),
            "FromJs dictionaries require named fields",
        ));
    };

    struct MemberPlan<'a> {
        field_ident: &'a Ident,
        field_ty: &'a syn::Type,
        options: MemberOptions,
        optional: bool,
    }
    let mut members = Vec::new();
    for field in &fields.named {
        let options = member_options(field)?;
        members.push(MemberPlan {
            field_ident: field.ident.as_ref().expect("named field"),
            field_ty: &field.ty,
            optional: type_is_option(&field.ty),
            options,
        });
    }
    // WebIDL reads dictionary members in lexicographic order — the
    // observable getter/proxy-trap order must not depend on Rust
    // field order.
    members.sort_by(|a, b| a.options.js_name.cmp(&b.options.js_name));

    let reads = members.iter().map(|member| {
        let field_ident = member.field_ident;
        let field_ty = member.field_ty;
        let js_name = LitStr::new(&member.options.js_name, field_ident.span());
        let missing = if member.options.default {
            quote!(::core::default::Default::default())
        } else if member.optional {
            quote!(::core::option::Option::None)
        } else {
            quote! {
                return ::core::result::Result::Err(
                    ::otter_vm::marshal::JsError::Type(::std::format!(
                        "{ident}: required member '{}' is missing",
                        #js_name,
                    )),
                )
            }
        };
        quote! {
            let #field_ident: #field_ty = if __nullish {
                #missing
            } else {
                let __member = cx.get(v, #js_name)?;
                if cx.is_undefined(__member) {
                    #missing
                } else {
                    ::otter_vm::marshal::FromJs::from_js(
                        cx,
                        __member,
                        ::otter_vm::marshal::ValueIdent::Member(#js_name),
                    )?
                }
            };
        }
    });
    let field_idents: Vec<_> = members.iter().map(|m| m.field_ident).collect();
    let ident = &input.ident;
    Ok(quote! {
        impl<'s> ::otter_vm::marshal::FromJs<'s> for #ident {
            fn from_js(
                cx: &mut ::otter_vm::marshal::MarshalCx<'_, '_, 's>,
                v: ::otter_vm::marshal::JsValue<'s>,
                ident: ::otter_vm::marshal::ValueIdent<'_>,
            ) -> ::core::result::Result<Self, ::otter_vm::marshal::JsError> {
                let __nullish = cx.is_nullish(v);
                #(#reads)*
                let _ = ident;
                ::core::result::Result::Ok(Self { #(#field_idents),* })
            }
        }
    })
}

fn expand_union(input: &DeriveInput, data: &syn::DataEnum) -> Result<proc_macro2::TokenStream> {
    if !input.generics.params.is_empty() {
        return Err(Error::new(
            input.generics.span(),
            "FromJs unions do not support generics",
        ));
    }
    if data.variants.is_empty() {
        return Err(Error::new(input.span(), "FromJs unions need variants"));
    }
    let mut arms = Vec::new();
    let last_index = data.variants.len() - 1;
    for (index, variant) in data.variants.iter().enumerate() {
        let Fields::Unnamed(fields) = &variant.fields else {
            return Err(Error::new(
                variant.span(),
                "FromJs union variants hold exactly one unnamed field",
            ));
        };
        if fields.unnamed.len() != 1 {
            return Err(Error::new(
                variant.span(),
                "FromJs union variants hold exactly one unnamed field",
            ));
        }
        let variant_ident = &variant.ident;
        let inner_ty = &fields.unnamed.first().expect("checked above").ty;
        if index == last_index {
            // Catch-all: unconditional conversion (a coercing type —
            // string, number — belongs here per WebIDL ordering).
            arms.push(quote! {
                ::otter_vm::marshal::FromJs::from_js(cx, v, ident).map(Self::#variant_ident)
            });
        } else {
            arms.push(quote! {
                if <#inner_ty as ::otter_vm::marshal::JsUnionProbe>::probe(cx, v) {
                    return ::otter_vm::marshal::FromJs::from_js(cx, v, ident)
                        .map(Self::#variant_ident);
                }
            });
        }
    }
    let ident = &input.ident;
    Ok(quote! {
        impl<'s> ::otter_vm::marshal::FromJs<'s> for #ident {
            fn from_js(
                cx: &mut ::otter_vm::marshal::MarshalCx<'_, '_, 's>,
                v: ::otter_vm::marshal::JsValue<'s>,
                ident: ::otter_vm::marshal::ValueIdent<'_>,
            ) -> ::core::result::Result<Self, ::otter_vm::marshal::JsError> {
                #(#arms)*
            }
        }
    })
}
