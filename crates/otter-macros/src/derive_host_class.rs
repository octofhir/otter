//! `#[derive(HostClass)]` — ancestry wiring for host-class data.
//!
//! Implements `otter_vm::marshal::HostAncestry` for a class's data
//! struct. Without a parent the derive emits the default (self-only)
//! walk. Marking one field `#[host_class(parent)]` chains the walk
//! into that field, which is what lets a base-class method
//! (`Blob.prototype.slice`) resolve its data on a subclass instance
//! (`File`): the field's own `HostAncestry` impl continues the chain,
//! so arbitrarily deep native hierarchies compose.
//!
//! Marking one field `#[host_class(bytes)]` declares the class to *be*
//! that byte sequence, which is what lets a native read a `Blob`'s
//! bytes without naming the Rust type. A class with only a parent
//! inherits the parent's answer, so `File` reads as its `Blob`.
//!
//! # Surface
//!
//! ```rust,ignore
//! #[derive(Clone, HostClass)]
//! pub struct File {
//!     #[host_class(parent)]
//!     blob: Blob,
//!     name: String,
//!     last_modified: f64,
//! }
//! ```
//!
//! # Invariants
//! - At most one field carries `#[host_class(parent)]`, and at most
//!   one `#[host_class(bytes)]`.
//! - A `bytes` field derefs to `[u8]`; the emitted view borrows from
//!   `self`, never from a temporary.
//! - The emitted walk returns views into `self` only — the
//!   `HostAncestry` contract the cell caster relies on.
//!
//! # See also
//! - [`crate::js_class`](super::js_class) — the impl-block attribute
//!   that consumes the ancestry.

use proc_macro::TokenStream;
use quote::quote;
use syn::spanned::Spanned;
use syn::{Data, DeriveInput, Error, Fields, Result};

/// Expand `#[derive(HostClass)]`.
pub(crate) fn expand(input: TokenStream) -> TokenStream {
    let input = syn::parse_macro_input!(input as DeriveInput);
    match expand_inner(&input) {
        Ok(tokens) => tokens.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

fn expand_inner(input: &DeriveInput) -> Result<proc_macro2::TokenStream> {
    let Data::Struct(data) = &input.data else {
        return Err(Error::new(
            input.span(),
            "HostClass derives on structs (class data types)",
        ));
    };
    if !input.generics.params.is_empty() {
        return Err(Error::new(
            input.generics.span(),
            "HostClass does not support generic host-data types",
        ));
    }
    let Fields::Named(fields) = &data.fields else {
        return Err(Error::new(
            data.fields.span(),
            "HostClass requires named fields",
        ));
    };

    let mut parent_field = None;
    let mut bytes_field = None;
    for field in &fields.named {
        for attr in &field.attrs {
            if !attr.path().is_ident("host_class") {
                continue;
            }
            let mut is_parent = false;
            let mut is_bytes = false;
            attr.parse_nested_meta(|meta| {
                if meta.path.is_ident("parent") {
                    is_parent = true;
                    Ok(())
                } else if meta.path.is_ident("bytes") {
                    is_bytes = true;
                    Ok(())
                } else {
                    Err(meta.error("host_class supports only `parent` and `bytes`"))
                }
            })?;
            if is_bytes {
                if bytes_field.is_some() {
                    return Err(Error::new(
                        field.span(),
                        "HostClass allows at most one #[host_class(bytes)] field",
                    ));
                }
                bytes_field = Some(
                    field
                        .ident
                        .clone()
                        .ok_or_else(|| Error::new(field.span(), "bytes field must be named"))?,
                );
            }
            if is_parent {
                if parent_field.is_some() {
                    return Err(Error::new(
                        field.span(),
                        "HostClass allows at most one #[host_class(parent)] field",
                    ));
                }
                parent_field = Some(
                    field
                        .ident
                        .clone()
                        .ok_or_else(|| Error::new(field.span(), "parent field must be named"))?,
                );
            }
        }
    }

    let ident = &input.ident;
    // A class that names its own byte field is that byte sequence; one that
    // only has a parent inherits whatever the parent is (a `File` reads as
    // its `Blob`); anything else is not a byte sequence at all.
    let host_bytes = match (&bytes_field, &parent_field) {
        (Some(bytes), _) => quote! {
            fn host_bytes(&self) -> ::core::option::Option<&[u8]> {
                ::core::option::Option::Some(::core::convert::AsRef::<[u8]>::as_ref(&self.#bytes))
            }
        },
        (None, Some(parent)) => quote! {
            fn host_bytes(&self) -> ::core::option::Option<&[u8]> {
                ::otter_vm::__macro_support::marshal::HostAncestry::host_bytes(&self.#parent)
            }
        },
        (None, None) => quote! {},
    };
    let body = match parent_field {
        Some(parent) => quote! {
            impl ::otter_vm::__macro_support::marshal::HostAncestry for #ident {
                #host_bytes

                fn ancestor(
                    &self,
                    target: ::core::any::TypeId,
                ) -> ::core::option::Option<&dyn ::core::any::Any> {
                    if target == ::core::any::TypeId::of::<Self>() {
                        ::core::option::Option::Some(self)
                    } else {
                        ::otter_vm::__macro_support::marshal::HostAncestry::ancestor(&self.#parent, target)
                    }
                }

                fn ancestor_mut(
                    &mut self,
                    target: ::core::any::TypeId,
                ) -> ::core::option::Option<&mut dyn ::core::any::Any> {
                    if target == ::core::any::TypeId::of::<Self>() {
                        ::core::option::Option::Some(self)
                    } else {
                        ::otter_vm::__macro_support::marshal::HostAncestry::ancestor_mut(&mut self.#parent, target)
                    }
                }
            }
        },
        None => quote! {
            impl ::otter_vm::__macro_support::marshal::HostAncestry for #ident {
                #host_bytes
            }
        },
    };
    Ok(body)
}
