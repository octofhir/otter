//! `#[derive(Pelt)]` proc macro — `SafeTraceable` for GC bodies.
//!
//! Derive expands to a single `impl ::otter_gc::SafeTraceable` block.
//! Every field that is not annotated with `#[pelt(skip)]` is funneled
//! through [`::otter_vm::pelt::PeltField::pelt_trace`]; missing
//! `PeltField` impls surface as ordinary compile errors at the field
//! span, satisfying the "fail loudly on untraceable fields"
//! acceptance gate for Phase 6.3.
//!
//! # Contents
//!
//! - [`expand`] — entry called from the proc-macro shim in
//!   [`crate::pelt_derive`](super).
//! - [`Args`] — parsed top-level `#[pelt(...)]` attributes (`tag` is
//!   required).
//!
//! # Invariants
//!
//! - The derive only supports `struct` items (named or tuple). Enums
//!   require a hand-written trace and are rejected with a clear
//!   message.
//! - The `#[pelt(tag = CONST)]` attribute is required. Reusing the
//!   already-existing per-body `_TYPE_TAG` const keeps tag
//!   coordination centralised in the body's own module.
//! - The expansion never references items outside `::otter_gc::*` and
//!   `::otter_vm::pelt::*`, so downstream crates do not need to import
//!   anything beyond the derive itself.
//!
//! # See also
//!
//! - [`otter_vm::pelt`] — `PeltField` trait + blanket impls.
//! - [`docs/otter-macros-design.md`](../../../docs/otter-macros-design.md).

use proc_macro::TokenStream;
use proc_macro2::Span;
use quote::quote;
use syn::parse::ParseStream;
use syn::{
    Attribute, Data, DataStruct, DeriveInput, Expr, Fields, Ident, Index, Path, Result, Token,
    parse_macro_input,
};

pub(crate) fn expand(input: TokenStream) -> TokenStream {
    let derive_input = parse_macro_input!(input as DeriveInput);

    let args = match parse_top_attrs(&derive_input.attrs) {
        Ok(a) => a,
        Err(err) => return err.to_compile_error().into(),
    };

    let tag = match args.tag {
        Some(t) => t,
        None => {
            return syn::Error::new(
                Span::call_site(),
                "missing required `#[pelt(tag = <CONST>)]` attribute on the body",
            )
            .to_compile_error()
            .into();
        }
    };

    let DataStruct { fields, .. } = match derive_input.data {
        Data::Struct(s) => s,
        Data::Enum(_) | Data::Union(_) => {
            return syn::Error::new(
                Span::call_site(),
                "#[derive(Pelt)] only supports structs; enums and unions need hand-written \
                 SafeTraceable impls",
            )
            .to_compile_error()
            .into();
        }
    };

    let body_ident = derive_input.ident;
    let trace_calls = match field_calls(&fields) {
        Ok(calls) => calls,
        Err(err) => return err.to_compile_error().into(),
    };

    let trace_body = if trace_calls.is_empty() {
        quote! { let _ = visitor; }
    } else {
        quote! { #(#trace_calls)* }
    };

    let ephemeron_fn = args.ephemeron_via.as_ref().map(|path| {
        quote! {
            fn trace_ephemeron_slots_safe(
                &mut self,
                visitor: &mut ::otter_gc::trace::EphemeronVisitor<'_>,
            ) {
                #path(self, visitor);
            }
        }
    });

    quote! {
        impl ::otter_gc::SafeTraceable for #body_ident {
            const TYPE_TAG: u8 = #tag;

            fn trace_slots_safe(
                &self,
                visitor: &mut ::otter_gc::raw::SlotVisitor<'_>,
            ) {
                #trace_body
            }

            #ephemeron_fn
        }
    }
    .into()
}

fn parse_top_attrs(attrs: &[Attribute]) -> Result<Args> {
    let mut tag: Option<Expr> = None;
    let mut ephemeron_via: Option<Path> = None;
    for attr in attrs {
        if !attr.path().is_ident("pelt") {
            continue;
        }
        attr.parse_args_with(|input: ParseStream<'_>| -> Result<()> {
            while !input.is_empty() {
                let key: Ident = input.parse()?;
                if key == "tag" {
                    input.parse::<Token![=]>()?;
                    if tag.is_some() {
                        return Err(syn::Error::new(key.span(), "duplicate `tag`"));
                    }
                    tag = Some(input.parse()?);
                } else if key == "ephemeron_via" {
                    input.parse::<Token![=]>()?;
                    if ephemeron_via.is_some() {
                        return Err(syn::Error::new(key.span(), "duplicate `ephemeron_via`"));
                    }
                    ephemeron_via = Some(input.parse()?);
                } else {
                    return Err(syn::Error::new(
                        key.span(),
                        format!("unknown top-level pelt attribute `{key}`"),
                    ));
                }
                if input.peek(Token![,]) {
                    input.parse::<Token![,]>()?;
                }
            }
            Ok(())
        })?;
    }
    Ok(Args { tag, ephemeron_via })
}

struct Args {
    tag: Option<Expr>,
    ephemeron_via: Option<Path>,
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
                let access = quote! { &self.#name };
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
                let access = quote! { &self.#idx };
                calls.push(emit_field_call(&access, attrs.via.as_ref()));
            }
        }
        Fields::Unit => {}
    }
    Ok(calls)
}

fn emit_field_call(access: &proc_macro2::TokenStream, via: Option<&Path>) -> proc_macro2::TokenStream {
    match via {
        Some(path) => quote! { #path(#access, visitor); },
        None => quote! {
            <_ as ::otter_vm::pelt::PeltField>::pelt_trace(#access, visitor);
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
        if !attr.path().is_ident("pelt") {
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
                        format!("unknown per-field pelt attribute `{key}`"),
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
            "`#[pelt(skip)]` and `#[pelt(via = ...)]` are mutually exclusive on the same field",
        ));
    }
    Ok(out)
}
