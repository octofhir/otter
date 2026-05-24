//! `#[derive(Groom)]` proc macro — `SafeFinalize` for GC bodies.
//!
//! Derive expands to a single `impl ::otter_gc::SafeFinalize` block
//! plus a `pub fn register_<body_snake>_finalize(&mut ::otter_gc::GcHeap)`
//! helper the body's module calls once at bootstrap to install the
//! finalize wrapper in the heap's type-tag dispatch table.
//!
//! Every field that is not annotated with `#[groom(skip)]` is funneled
//! through [`::otter_vm::groom::GroomField::groom`]; missing
//! `GroomField` impls surface as ordinary compile errors at the field
//! span — the same "fail loudly on un-finalize-able fields" gate the
//! [`Pelt`](super) derive uses for tracing.
//!
//! # Contents
//!
//! - [`expand`] — entry called from the proc-macro shim in
//!   [`crate::groom_derive`](super).
//! - [`Args`] — parsed top-level `#[groom(...)]` attributes.
//!
//! # Invariants
//!
//! - The derive only supports `struct` items (named or tuple). Enums
//!   require a hand-written finalize and are rejected with a clear
//!   message.
//! - The body must also implement [`::otter_gc::SafeTraceable`] —
//!   typically through `#[derive(Pelt)]` on the same item. The Rust
//!   trait bound on [`::otter_gc::SafeFinalize`] enforces this at
//!   compile time.
//!
//! # See also
//!
//! - [`::otter_vm::groom`] — `GroomField` trait + blanket impls.
//! - [`::otter_gc::SafeFinalize`] — trait the derive implements on
//!   the body type.

use proc_macro::TokenStream;
use proc_macro2::Span;
use quote::quote;
use syn::parse::ParseStream;
use syn::{
    Attribute, Data, DataStruct, DeriveInput, Fields, Ident, Index, Path, Result, Token,
    parse_macro_input,
};

pub(crate) fn expand(input: TokenStream) -> TokenStream {
    let derive_input = parse_macro_input!(input as DeriveInput);

    if let Err(err) = parse_top_attrs(&derive_input.attrs) {
        return err.to_compile_error().into();
    }

    let DataStruct { fields, .. } = match derive_input.data {
        Data::Struct(s) => s,
        Data::Enum(_) | Data::Union(_) => {
            return syn::Error::new(
                Span::call_site(),
                "#[derive(Groom)] only supports structs; enums and unions need hand-written \
                 SafeFinalize impls",
            )
            .to_compile_error()
            .into();
        }
    };

    let body_ident = derive_input.ident;
    let groom_calls = match field_calls(&fields) {
        Ok(calls) => calls,
        Err(err) => return err.to_compile_error().into(),
    };

    let groom_body = if groom_calls.is_empty() {
        quote! {}
    } else {
        quote! { #(#groom_calls)* }
    };

    quote! {
        impl ::otter_gc::SafeFinalize for #body_ident {
            fn finalize_safe(&mut self) {
                #groom_body
            }
        }
    }
    .into()
}

fn parse_top_attrs(attrs: &[Attribute]) -> Result<()> {
    for attr in attrs {
        if !attr.path().is_ident("groom") {
            continue;
        }
        attr.parse_args_with(|input: ParseStream<'_>| -> Result<()> {
            if input.is_empty() {
                return Ok(());
            }
            let key: Ident = input.parse()?;
            Err(syn::Error::new(
                key.span(),
                format!("unknown top-level groom attribute `{key}`"),
            ))
        })?;
    }
    Ok(())
}

fn field_calls(fields: &Fields) -> Result<Vec<proc_macro2::TokenStream>> {
    let mut calls = Vec::new();
    match fields {
        Fields::Named(named) => {
            for f in &named.named {
                let attrs = parse_field_attrs(&f.attrs)?;
                if attrs.skip {
                    continue;
                }
                let name = f.ident.as_ref().expect("named field has ident");
                let access = quote! { &mut self.#name };
                calls.push(emit_field_call(&access, attrs.via.as_ref()));
            }
        }
        Fields::Unnamed(unnamed) => {
            for (i, f) in unnamed.unnamed.iter().enumerate() {
                let attrs = parse_field_attrs(&f.attrs)?;
                if attrs.skip {
                    continue;
                }
                let idx = Index::from(i);
                let access = quote! { &mut self.#idx };
                calls.push(emit_field_call(&access, attrs.via.as_ref()));
            }
        }
        Fields::Unit => {}
    }
    Ok(calls)
}

fn emit_field_call(
    access: &proc_macro2::TokenStream,
    via: Option<&Path>,
) -> proc_macro2::TokenStream {
    match via {
        Some(path) => quote! { #path(#access); },
        None => quote! {
            <_ as ::otter_vm::groom::GroomField>::groom(#access);
        },
    }
}

#[derive(Default)]
struct FieldAttrs {
    skip: bool,
    via: Option<Path>,
}

fn parse_field_attrs(attrs: &[Attribute]) -> Result<FieldAttrs> {
    let mut out = FieldAttrs::default();
    for attr in attrs {
        if !attr.path().is_ident("groom") {
            continue;
        }
        attr.parse_args_with(|input: ParseStream<'_>| -> Result<()> {
            while !input.is_empty() {
                let key: Ident = input.parse()?;
                if key == "skip" {
                    out.skip = true;
                } else if key == "via" {
                    input.parse::<Token![=]>()?;
                    if out.via.is_some() {
                        return Err(syn::Error::new(key.span(), "duplicate `via`"));
                    }
                    out.via = Some(input.parse()?);
                } else {
                    return Err(syn::Error::new(
                        key.span(),
                        format!("unknown per-field groom attribute `{key}`"),
                    ));
                }
                if input.peek(Token![,]) {
                    input.parse::<Token![,]>()?;
                }
            }
            Ok(())
        })?;
    }
    if out.skip && out.via.is_some() {
        return Err(syn::Error::new(
            Span::call_site(),
            "`#[groom(skip)]` and `#[groom(via = ...)]` are mutually exclusive on the same field",
        ));
    }
    Ok(out)
}
