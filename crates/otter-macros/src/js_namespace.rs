use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::{ItemImpl, ItemStruct};

use crate::{JsClassArgs, JsMemberKind, is_active_js_class_method, parse_js_member_attr};

pub(crate) fn expand_js_namespace_struct(input: ItemStruct, args: JsClassArgs) -> TokenStream {
    let struct_name = &input.ident;
    let vis = &input.vis;
    let attrs = &input.attrs;
    let generics = &input.generics;
    let namespace_name = args.name.unwrap_or_else(|| struct_name.to_string());

    let mut cleaned_fields = Vec::new();
    for field in input.fields.iter() {
        let cleaned_attrs: Vec<_> = field
            .attrs
            .iter()
            .filter(|a| !a.path().is_ident("js_skip") && !a.path().is_ident("js_readonly"))
            .collect();
        let field_vis = &field.vis;
        let field_ident = &field.ident;
        let field_ty = &field.ty;
        cleaned_fields.push(quote! {
            #(#cleaned_attrs)*
            #field_vis #field_ident: #field_ty
        });
    }

    TokenStream::from(quote! {
        #(#attrs)*
        #vis struct #struct_name #generics {
            #(#cleaned_fields),*
        }

        impl #struct_name {
            /// JavaScript namespace name.
            pub const JS_NAMESPACE_NAME: &'static str = #namespace_name;
        }
    })
}

pub(crate) fn expand_js_namespace_impl(input: ItemImpl) -> TokenStream {
    let self_ty = &input.self_ty;
    let mut errors = Vec::new();

    let mut methods = Vec::new();
    let mut js_getters = Vec::new();
    let mut js_setters = Vec::new();

    struct DescriptorInfo {
        rust_ident: syn::Ident,
        js_name: String,
        length: u32,
        kind: JsMemberKind,
    }

    let mut descriptor_members: Vec<DescriptorInfo> = Vec::new();

    for item in &input.items {
        if let syn::ImplItem::Fn(method) = item {
            let rust_ident = method.sig.ident.clone();
            let rust_name = rust_ident.to_string();
            let mut member_attr = None;

            for attr in &method.attrs {
                match parse_js_member_attr(attr) {
                    Ok(Some(parsed_attr)) => {
                        if member_attr.is_some() {
                            errors.push(
                                syn::Error::new_spanned(
                                    attr,
                                    "Expected at most one js_namespace member attribute per method.",
                                )
                                .to_compile_error(),
                            );
                            continue;
                        }
                        member_attr = Some(parsed_attr);
                    }
                    Ok(None) => {}
                    Err(error) => errors.push(error.to_compile_error()),
                }
            }

            let Some(member_attr) = member_attr else {
                continue;
            };

            match member_attr.kind {
                JsMemberKind::Method | JsMemberKind::Getter | JsMemberKind::Setter => {}
                JsMemberKind::Constructor | JsMemberKind::Static => {
                    errors.push(
                        syn::Error::new_spanned(
                            &method.sig.ident,
                            "js_namespace only supports #[js_method], #[js_getter], and #[js_setter].",
                        )
                        .to_compile_error(),
                    );
                    continue;
                }
            }

            if !is_active_js_class_method(method) {
                errors.push(
                    syn::Error::new_spanned(
                        &method.sig.ident,
                        "js_namespace only supports active runtime methods with signature fn(&RegisterValue, &[RegisterValue], &mut RuntimeState) -> Result<RegisterValue, VmNativeCallError>.",
                    )
                    .to_compile_error(),
                );
                continue;
            }

            let js_name = member_attr.name.unwrap_or_else(|| rust_name.clone());
            let length = member_attr
                .length
                .unwrap_or_else(|| member_attr.kind.default_length());

            match member_attr.kind {
                JsMemberKind::Method => methods.push(rust_name.clone()),
                JsMemberKind::Getter => js_getters.push(rust_name.clone()),
                JsMemberKind::Setter => js_setters.push(rust_name.clone()),
                JsMemberKind::Constructor | JsMemberKind::Static => unreachable!(),
            }

            descriptor_members.push(DescriptorInfo {
                rust_ident,
                js_name,
                length,
                kind: member_attr.kind,
            });
        }
    }

    if !errors.is_empty() {
        return quote! {
            #(#errors)*
            #input
        }
        .into();
    }

    let descriptor_fns: Vec<_> = descriptor_members
        .iter()
        .map(|info| {
            let descriptor_fn_name = format_ident!("{}_descriptor", info.rust_ident);
            let rust_ident = &info.rust_ident;
            let js_name = &info.js_name;
            let length = info.length;

            let descriptor_ctor = match info.kind {
                JsMemberKind::Method => {
                    quote! {
                        ::otter_vm::NativeFunctionDescriptor::method(
                            #js_name,
                            #length as u16,
                            callback,
                        )
                    }
                }
                JsMemberKind::Getter => {
                    quote! {
                        ::otter_vm::NativeFunctionDescriptor::getter(#js_name, callback)
                    }
                }
                JsMemberKind::Setter => {
                    quote! {
                        ::otter_vm::NativeFunctionDescriptor::setter(#js_name, callback)
                    }
                }
                JsMemberKind::Constructor | JsMemberKind::Static => unreachable!(),
            };

            quote! {
                /// Descriptor for this js_namespace member.
                pub fn #descriptor_fn_name() -> ::otter_vm::NativeFunctionDescriptor {
                    let callback = Self::#rust_ident as ::otter_vm::VmNativeFunction;
                    #descriptor_ctor
                }
            }
        })
        .collect();

    let binding_pushes: Vec<_> = descriptor_members
        .iter()
        .map(|info| {
            let descriptor_fn_name = format_ident!("{}_descriptor", info.rust_ident);
            quote! {
                descriptor = descriptor.with_binding(::otter_vm::NativeBindingDescriptor::new(
                    ::otter_vm::NativeBindingTarget::Namespace,
                    Self::#descriptor_fn_name(),
                ));
            }
        })
        .collect();

    TokenStream::from(quote! {
        #input

        impl #self_ty {
            /// Get JS namespace method names.
            pub fn js_namespace_methods() -> &'static [&'static str] {
                &[#(#methods),*]
            }

            /// Get JS namespace getter names.
            pub fn js_namespace_getters() -> &'static [&'static str] {
                &[#(#js_getters),*]
            }

            /// Get JS namespace setter names.
            pub fn js_namespace_setters() -> &'static [&'static str] {
                &[#(#js_setters),*]
            }

            #(#descriptor_fns)*

            /// Aggregate namespace descriptor emitted by #[js_namespace].
            pub fn js_namespace_descriptor() -> ::otter_vm::JsNamespaceDescriptor {
                let mut descriptor =
                    ::otter_vm::JsNamespaceDescriptor::new(Self::JS_NAMESPACE_NAME);
                #(#binding_pushes)*
                descriptor
            }
        }
    })
}
