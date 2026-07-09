//! `#[js_module]` attribute macro — declarative hosted-module
//! generator.
//!
//! The hosted-module counterpart of [`crate::js_namespace`]: an
//! inherent `impl` block on a marker type whose signatures are the
//! module's export descriptor. Exports are static (no receiver);
//! parameter types drive `FromJs` extraction, return types drive
//! `IntoJs` construction, `async fn` compiles to the promise
//! protocol. Expansion emits per-export glue plus a `lodge!`
//! invocation, so the installer and the `<UPPER>_HOSTED_MODULE` row
//! stay on the proven machinery.
//!
//! # Surface
//!
//! ```rust,ignore
//! pub struct KvModule;
//!
//! #[js_module(prefix = "otter", name = "kv", capabilities = true)]
//! impl KvModule {
//!     #[export(name = "openKv")]
//!     fn open_kv(caps: &CapabilitySet, path: Option<USVString>) -> Result<KvStore, JsError> { … }
//!
//!     #[export(name = "fetchRows")]
//!     async fn fetch_rows(query: USVString) -> Result<Vec<Row>, JsError> { … }
//!
//!     #[export(name = "fastPath", length = 1, raw)]
//!     fn fast(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> { … }
//! }
//! ```
//!
//! Module-level arguments: `prefix = "otter"` **or** `specifier =
//! "otter"` (exact), `name = "kv"` (both explicit — the module
//! specifier is never inferred), `capabilities = true` to thread the
//! install-time [`CapabilitySet`] snapshot. With capabilities on, an
//! export may declare `caps: &CapabilitySet` as its FIRST parameter
//! to receive the snapshot; exports that don't need it simply omit
//! the parameter. `raw` exports use the lodge-native signature for
//! their capability mode.
//!
//! Export markers: `#[export(name = "…")]` with `length = N`,
//! `promise`, `raw` — same semantics as `js_namespace` methods.
//!
//! # Invariants
//! - JS export names and the module specifier are explicit at the
//!   declaration site.
//! - Generated glue runs one `ctx.scope` + `MarshalCx` per call,
//!   inheriting the handle-scope rooting contract.
//!
//! # See also
//! - [`crate::lodge`](super::lodge) — the installer machinery.
//! - `EXTENSION_API_PLAN.md` §4 — the design.

use proc_macro::TokenStream;
use proc_macro2::Span;
use quote::{format_ident, quote};
use syn::parse::{Parse, ParseStream};
use syn::spanned::Spanned;
use syn::{
    Error, FnArg, Ident, ImplItem, ImplItemFn, ItemImpl, LitBool, LitInt, LitStr, Result,
    ReturnType, Token, Type,
};

/// Parsed `#[js_module(...)]` arguments.
pub(crate) struct ModuleArgs {
    prefix: Option<LitStr>,
    specifier: Option<LitStr>,
    name: LitStr,
    capabilities: bool,
}

impl Parse for ModuleArgs {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let mut prefix: Option<LitStr> = None;
        let mut specifier: Option<LitStr> = None;
        let mut name: Option<LitStr> = None;
        let mut capabilities = false;
        while !input.is_empty() {
            let key: Ident = input.parse()?;
            input.parse::<Token![=]>()?;
            match key.to_string().as_str() {
                "prefix" => prefix = Some(input.parse()?),
                "specifier" => specifier = Some(input.parse()?),
                "name" => name = Some(input.parse()?),
                "capabilities" => {
                    let lit: LitBool = input.parse()?;
                    capabilities = lit.value;
                }
                other => {
                    return Err(Error::new(
                        key.span(),
                        format!(
                            "unknown js_module option `{other}`; expected \
                             `prefix`, `specifier`, `name`, or `capabilities`"
                        ),
                    ));
                }
            }
            if input.peek(Token![,]) {
                input.parse::<Token![,]>()?;
            }
        }
        Ok(Self {
            prefix,
            specifier,
            name: name.ok_or_else(|| {
                Error::new(Span::call_site(), "js_module requires `name = \"…\"`")
            })?,
            capabilities,
        })
    }
}

struct Export {
    js_name: LitStr,
    length: Option<u8>,
    promise: bool,
    raw: bool,
    fn_ident: Ident,
    /// Non-caps parameter types, in order.
    params: Vec<Type>,
    /// Whether the fn's first parameter is the capability snapshot.
    takes_caps: bool,
    returns_result: bool,
    is_async: bool,
}

fn classify(item: &mut ImplItemFn) -> Result<Option<(LitStr, Option<u8>, bool, bool)>> {
    let mut found = None;
    let mut keep = Vec::with_capacity(item.attrs.len());
    for attr in item.attrs.drain(..) {
        if attr.path().is_ident("export") {
            let mut js_name: Option<LitStr> = None;
            let mut length: Option<u8> = None;
            let mut promise = false;
            let mut raw = false;
            attr.parse_nested_meta(|meta| {
                if meta.path.is_ident("name") {
                    js_name = Some(meta.value()?.parse()?);
                } else if meta.path.is_ident("length") {
                    let lit: LitInt = meta.value()?.parse()?;
                    length = Some(lit.base10_parse()?);
                } else if meta.path.is_ident("promise") {
                    promise = true;
                } else if meta.path.is_ident("raw") {
                    raw = true;
                } else {
                    return Err(meta
                        .error("export supports `name = \"…\"`, `length = N`, `promise`, `raw`"));
                }
                Ok(())
            })?;
            let js_name = js_name.ok_or_else(|| {
                Error::new(attr.span(), "export requires an explicit `name = \"…\"`")
            })?;
            if found.is_some() {
                return Err(Error::new(attr.span(), "export has more than one marker"));
            }
            found = Some((js_name, length, promise, raw));
        } else {
            keep.push(attr);
        }
    }
    item.attrs = keep;
    Ok(found)
}

fn type_is_result(ty: &Type) -> bool {
    let Type::Path(path) = ty else { return false };
    path.path
        .segments
        .last()
        .is_some_and(|segment| segment.ident == "Result")
}

fn type_is_capability_set(ty: &Type) -> bool {
    let Type::Reference(reference) = ty else {
        return false;
    };
    let Type::Path(path) = reference.elem.as_ref() else {
        return false;
    };
    path.path
        .segments
        .last()
        .is_some_and(|segment| segment.ident == "CapabilitySet")
}

fn default_length(params: &[Type]) -> u8 {
    let required = params
        .iter()
        .take_while(|ty| {
            let Type::Path(path) = ty else { return true };
            path.path
                .segments
                .last()
                .is_none_or(|segment| segment.ident != "Option")
        })
        .count();
    u8::try_from(required).unwrap_or(u8::MAX)
}

fn arg_extraction(index: usize, ty: &Type, op: &LitStr) -> proc_macro2::TokenStream {
    let arg = format_ident!("__arg_{index}");
    let handle = format_ident!("__arg_handle_{index}");
    quote! {
        let #handle = __cx.park(
            args.get(#index)
                .copied()
                .unwrap_or_else(::otter_runtime::Value::undefined),
        );
        let #arg: #ty = ::otter_runtime::marshal::FromJs::from_js(
            &mut __cx,
            #handle,
            ::otter_runtime::marshal::ValueIdent::Argument(#index),
        )
        .map_err(|e| e.into_native(#op))?;
    }
}

/// Expand `#[js_module]` — see the module docs.
pub(crate) fn expand(attr: TokenStream, item: TokenStream) -> TokenStream {
    let args = syn::parse_macro_input!(attr as ModuleArgs);
    let mut module_impl = syn::parse_macro_input!(item as ItemImpl);
    match expand_inner(&args, &mut module_impl) {
        Ok(generated) => {
            let mut out = quote!(#module_impl);
            out.extend(generated);
            out.into()
        }
        Err(err) => err.to_compile_error().into(),
    }
}

fn expand_inner(args: &ModuleArgs, module_impl: &mut ItemImpl) -> Result<proc_macro2::TokenStream> {
    if module_impl.trait_.is_some() || !module_impl.generics.params.is_empty() {
        return Err(Error::new(
            module_impl.span(),
            "js_module goes on a plain inherent impl block",
        ));
    }
    let self_ty = (*module_impl.self_ty).clone();
    let Type::Path(self_path) = &self_ty else {
        return Err(Error::new(
            module_impl.self_ty.span(),
            "js_module requires a plain type name",
        ));
    };
    let type_ident = self_path
        .path
        .segments
        .last()
        .map(|segment| segment.ident.clone())
        .ok_or_else(|| Error::new(self_path.span(), "js_module requires a plain type name"))?;
    let glue_prefix = type_ident.to_string().to_lowercase();
    let module_name = &args.name;
    let with_caps = args.capabilities;

    let mut exports = Vec::new();
    for item in &mut module_impl.items {
        if let ImplItem::Fn(fn_item) = item
            && let Some((js_name, length, promise, raw)) = classify(fn_item)?
        {
            if fn_item.sig.receiver().is_some() {
                return Err(Error::new(
                    fn_item.sig.span(),
                    "module exports are static — no receiver",
                ));
            }
            let mut params: Vec<Type> = fn_item
                .sig
                .inputs
                .iter()
                .filter_map(|input| match input {
                    FnArg::Typed(arg) => Some((*arg.ty).clone()),
                    FnArg::Receiver(_) => None,
                })
                .collect();
            let takes_caps = !raw && params.first().is_some_and(type_is_capability_set);
            if takes_caps {
                if !with_caps {
                    return Err(Error::new(
                        fn_item.sig.span(),
                        "a `caps: &CapabilitySet` parameter requires \
                         `capabilities = true` on the module",
                    ));
                }
                params.remove(0);
            }
            let returns_result = match &fn_item.sig.output {
                ReturnType::Default => false,
                ReturnType::Type(_, ty) => type_is_result(ty),
            };
            exports.push(Export {
                js_name,
                length,
                promise,
                raw,
                fn_ident: fn_item.sig.ident.clone(),
                params,
                takes_caps,
                returns_result,
                is_async: fn_item.sig.asyncness.is_some(),
            });
        }
    }

    let mut glue = proc_macro2::TokenStream::new();
    let mut export_rows = Vec::new();
    for export in &exports {
        let js_name = &export.js_name;
        let length = export
            .length
            .unwrap_or_else(|| default_length(&export.params));
        let length_lit = LitInt::new(&length.to_string(), Span::call_site());
        let fn_ident = &export.fn_ident;

        if export.raw {
            let type_path = &self_path.path;
            export_rows.push(quote!(#js_name / #length_lit => #type_path::#fn_ident));
            continue;
        }
        if export.promise && export.is_async {
            return Err(Error::new(
                fn_ident.span(),
                "async exports already return a promise; drop `promise`",
            ));
        }

        let op_string = format!("{}:{}.{}", "module", module_name.value(), js_name.value());
        let op = LitStr::new(&op_string, js_name.span());
        let glue_ident = format_ident!("__otter_js_module_{glue_prefix}_{fn_ident}");
        let extractions: Vec<_> = export
            .params
            .iter()
            .enumerate()
            .map(|(index, ty)| arg_extraction(index, ty, &op))
            .collect();
        let arg_names: Vec<Ident> = (0..export.params.len())
            .map(|i| format_ident!("__arg_{i}"))
            .collect();
        let caps_arg = if export.takes_caps {
            quote!(caps,)
        } else {
            quote!()
        };

        let output = if export.is_async {
            let future_expr = if export.returns_result {
                quote!(__call)
            } else {
                quote!(async move {
                    ::core::result::Result::<_, ::otter_runtime::marshal::JsError>::Ok(__call.await)
                })
            };
            quote! {
                let __call = <#self_ty>::#fn_ident(#caps_arg #(#arg_names),*);
                let __future = #future_expr;
                let __out = __cx
                    .promise_from_future(__future)
                    .map_err(|e| e.into_native(#op))?;
                ::core::result::Result::Ok(__cx.escape(__out))
            }
        } else {
            let body_call = quote!(<#self_ty>::#fn_ident(#caps_arg #(#arg_names),*));
            let normalized = if export.returns_result {
                quote!(#body_call.map_err(|e| e.into_native(#op))?)
            } else {
                body_call
            };
            let promise_wrap = if export.promise {
                quote! {
                    let __out = __cx
                        .promise_fulfilled(__out)
                        .map_err(|e| e.into_native(#op))?;
                }
            } else {
                quote!()
            };
            quote! {
                let __result = #normalized;
                let __out = ::otter_runtime::marshal::IntoJs::into_js(__result, &mut __cx)
                    .map_err(|e| e.into_native(#op))?;
                #promise_wrap
                ::core::result::Result::Ok(__cx.escape(__out))
            }
        };

        // The glue's outer signature depends on the module's
        // capability mode (lodge! wires all exports uniformly).
        let signature = if with_caps {
            quote! {
                fn #glue_ident(
                    ctx: &mut ::otter_runtime::NativeCtx<'_>,
                    args: &[::otter_runtime::Value],
                    caps: &::otter_runtime::CapabilitySet,
                ) -> ::core::result::Result<::otter_runtime::Value, ::otter_runtime::NativeError>
            }
        } else {
            quote! {
                fn #glue_ident(
                    ctx: &mut ::otter_runtime::NativeCtx<'_>,
                    args: &[::otter_runtime::Value],
                ) -> ::core::result::Result<::otter_runtime::Value, ::otter_runtime::NativeError>
            }
        };
        let silence_caps = if with_caps && !export.takes_caps {
            quote!(let _ = caps;)
        } else {
            quote!()
        };
        glue.extend(quote! {
            #signature {
                #silence_caps
                ctx.scope(|ctx, __s| {
                    let mut __cx = ::otter_runtime::marshal::MarshalCx::new(ctx, __s);
                    #(#extractions)*
                    #output
                })
            }
        });
        export_rows.push(quote!(#js_name / #length_lit => #glue_ident));
    }

    let capabilities_lit = LitBool::new(with_caps, Span::call_site());
    let source_field = match (&args.prefix, &args.specifier) {
        (Some(prefix), None) => quote!(prefix = #prefix,),
        (None, Some(specifier)) => quote!(specifier = #specifier,),
        _ => {
            return Err(Error::new(
                Span::call_site(),
                "js_module requires exactly one of `prefix = \"…\"` or `specifier = \"…\"`",
            ));
        }
    };
    glue.extend(quote! {
        ::otter_macros::lodge! {
            #source_field
            name = #module_name,
            capabilities = #capabilities_lit,
            exports = { #(#export_rows,)* },
        }
    });
    Ok(glue)
}
