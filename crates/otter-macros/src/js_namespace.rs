//! `#[js_namespace]` attribute macro — declarative namespace
//! generator.
//!
//! The namespace counterpart of [`crate::js_class`]: an inherent
//! `impl` block on a marker type whose signatures are the descriptor.
//! Members are static (no receiver); parameter types drive `FromJs`
//! extraction, return types drive `IntoJs` construction, `async fn`
//! compiles to the promise protocol. Expansion emits per-member glue
//! plus a `holt!` invocation, so specs, install, and the `Intrinsic`
//! handle stay on the proven machinery.
//!
//! # Surface
//!
//! ```rust,ignore
//! pub struct WebCrypto;
//!
//! #[js_namespace(name = "crypto", feature = WEB, tag = "Crypto", js = "crypto.ns.js")]
//! impl WebCrypto {
//!     #[method(name = "randomUUID")]
//!     fn random_uuid() -> Result<String, JsError> { … }
//!
//!     #[method(name = "digest")]
//!     async fn digest(algorithm: USVString, data: BufferSource)
//!         -> Result<ArrayBuffer, JsError> { … }
//!
//!     #[method(name = "fastPath", length = 1, raw)]
//!     fn fast(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> { … }
//! }
//! ```
//!
//! Class-level arguments: `name` (required, explicit JS name),
//! `feature` (required), `tag = "…"` (`@@toStringTag` on the
//! namespace object), `js = "…"` (co-located JS glue evaluated right
//! after the native install — the place for members that are
//! genuinely better in JS).
//!
//! Member markers: `#[method(name = "…")]` with `length = N`,
//! `promise`, `raw` options — same semantics as `js_class`, minus
//! receivers. `async fn` members take no receiver and return the
//! promise protocol.
//!
//! # Invariants
//! - JS names are explicit at the declaration site; never inferred.
//! - Generated glue runs one `ctx.scope` + `MarshalCx` per call,
//!   inheriting the handle-scope rooting contract.
//!
//! # See also
//! - [`crate::holt`](super::holt) — the spec/installer machinery.
//! - `EXTENSION_API_PLAN.md` §4 — the design.

use proc_macro::TokenStream;
use proc_macro2::Span;
use quote::{format_ident, quote};
use syn::parse::{Parse, ParseStream};
use syn::spanned::Spanned;
use syn::{
    Error, FnArg, Ident, ImplItem, ImplItemFn, ItemImpl, LitInt, LitStr, Result, ReturnType, Token,
    Type,
};

/// Parsed `#[js_namespace(...)]` arguments.
pub(crate) struct NamespaceArgs {
    name: LitStr,
    feature: Ident,
    tag: Option<LitStr>,
    js: Option<LitStr>,
}

impl Parse for NamespaceArgs {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let mut name: Option<LitStr> = None;
        let mut feature: Option<Ident> = None;
        let mut tag: Option<LitStr> = None;
        let mut js: Option<LitStr> = None;
        while !input.is_empty() {
            let key: Ident = input.parse()?;
            input.parse::<Token![=]>()?;
            match key.to_string().as_str() {
                "name" => name = Some(input.parse()?),
                "feature" => feature = Some(input.parse()?),
                "tag" => tag = Some(input.parse()?),
                "js" => js = Some(input.parse()?),
                other => {
                    return Err(Error::new(
                        key.span(),
                        format!(
                            "unknown js_namespace option `{other}`; expected \
                             `name`, `feature`, `tag`, or `js`"
                        ),
                    ));
                }
            }
            if input.peek(Token![,]) {
                input.parse::<Token![,]>()?;
            }
        }
        Ok(Self {
            name: name.ok_or_else(|| {
                Error::new(Span::call_site(), "js_namespace requires `name = \"…\"`")
            })?,
            feature: feature.ok_or_else(|| {
                Error::new(Span::call_site(), "js_namespace requires `feature = …`")
            })?,
            tag,
            js,
        })
    }
}

struct Member {
    js_name: LitStr,
    length: Option<u8>,
    promise: bool,
    raw: bool,
    fn_ident: Ident,
    params: Vec<Type>,
    returns_result: bool,
    is_async: bool,
}

fn classify(item: &mut ImplItemFn) -> Result<Option<(LitStr, Option<u8>, bool, bool)>> {
    let mut found = None;
    let mut keep = Vec::with_capacity(item.attrs.len());
    for attr in item.attrs.drain(..) {
        if attr.path().is_ident("method") {
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
                        .error("method supports `name = \"…\"`, `length = N`, `promise`, `raw`"));
                }
                Ok(())
            })?;
            let js_name = js_name.ok_or_else(|| {
                Error::new(attr.span(), "method requires an explicit `name = \"…\"`")
            })?;
            if found.is_some() {
                return Err(Error::new(attr.span(), "member has more than one marker"));
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
                .unwrap_or_else(::otter_vm::Value::undefined),
        );
        let #arg: #ty = ::otter_vm::marshal::FromJs::from_js(
            &mut __cx,
            #handle,
            ::otter_vm::marshal::ValueIdent::Argument(#index),
        )
        .map_err(|e| e.into_native(#op))?;
    }
}

/// Expand `#[js_namespace]` — see the module docs.
pub(crate) fn expand(attr: TokenStream, item: TokenStream) -> TokenStream {
    let args = syn::parse_macro_input!(attr as NamespaceArgs);
    let mut ns_impl = syn::parse_macro_input!(item as ItemImpl);
    match expand_inner(&args, &mut ns_impl) {
        Ok(generated) => {
            let mut out = quote!(#ns_impl);
            out.extend(generated);
            out.into()
        }
        Err(err) => err.to_compile_error().into(),
    }
}

fn expand_inner(args: &NamespaceArgs, ns_impl: &mut ItemImpl) -> Result<proc_macro2::TokenStream> {
    if ns_impl.trait_.is_some() || !ns_impl.generics.params.is_empty() {
        return Err(Error::new(
            ns_impl.span(),
            "js_namespace goes on a plain inherent impl block",
        ));
    }
    let self_ty = (*ns_impl.self_ty).clone();
    let Type::Path(self_path) = &self_ty else {
        return Err(Error::new(
            ns_impl.self_ty.span(),
            "js_namespace requires a plain type name",
        ));
    };
    let type_ident = self_path
        .path
        .segments
        .last()
        .map(|segment| segment.ident.clone())
        .ok_or_else(|| Error::new(self_path.span(), "js_namespace requires a plain type name"))?;
    let glue_prefix = type_ident.to_string().to_lowercase();
    let ns_name = &args.name;
    let feature = &args.feature;

    let mut members = Vec::new();
    for item in &mut ns_impl.items {
        if let ImplItem::Fn(fn_item) = item
            && let Some((js_name, length, promise, raw)) = classify(fn_item)?
        {
            if fn_item.sig.receiver().is_some() {
                return Err(Error::new(
                    fn_item.sig.span(),
                    "namespace members are static — no receiver",
                ));
            }
            let params: Vec<Type> = fn_item
                .sig
                .inputs
                .iter()
                .filter_map(|input| match input {
                    FnArg::Typed(arg) => Some((*arg.ty).clone()),
                    FnArg::Receiver(_) => None,
                })
                .collect();
            let returns_result = match &fn_item.sig.output {
                ReturnType::Default => false,
                ReturnType::Type(_, ty) => type_is_result(ty),
            };
            members.push(Member {
                js_name,
                length,
                promise,
                raw,
                fn_ident: fn_item.sig.ident.clone(),
                params,
                returns_result,
                is_async: fn_item.sig.asyncness.is_some(),
            });
        }
    }

    let mut glue = proc_macro2::TokenStream::new();
    let mut method_rows = Vec::new();
    for member in &members {
        let js_name = &member.js_name;
        let length = member
            .length
            .unwrap_or_else(|| default_length(&member.params));
        let length_lit = LitInt::new(&length.to_string(), Span::call_site());
        let fn_ident = &member.fn_ident;

        if member.raw {
            let type_path = &self_path.path;
            method_rows.push(quote!(#js_name / #length_lit => #type_path::#fn_ident));
            continue;
        }
        if member.promise && member.is_async {
            return Err(Error::new(
                fn_ident.span(),
                "async methods already return a promise; drop `promise`",
            ));
        }

        let op_string = format!("{}.{}", ns_name.value(), js_name.value());
        let op = LitStr::new(&op_string, js_name.span());
        let glue_ident = format_ident!("__otter_js_namespace_{glue_prefix}_{fn_ident}");
        let extractions: Vec<_> = member
            .params
            .iter()
            .enumerate()
            .map(|(index, ty)| arg_extraction(index, ty, &op))
            .collect();
        let arg_names: Vec<Ident> = (0..member.params.len())
            .map(|i| format_ident!("__arg_{i}"))
            .collect();

        let output = if member.is_async {
            let future_expr = if member.returns_result {
                quote!(__call)
            } else {
                quote!(async move {
                    ::core::result::Result::<_, ::otter_vm::marshal::JsError>::Ok(__call.await)
                })
            };
            quote! {
                let __call = <#self_ty>::#fn_ident(#(#arg_names),*);
                let __future = #future_expr;
                let __out = __cx
                    .promise_from_future(__future)
                    .map_err(|e| e.into_native(#op))?;
                ::core::result::Result::Ok(__cx.escape(__out))
            }
        } else {
            let body_call = quote!(<#self_ty>::#fn_ident(#(#arg_names),*));
            let normalized = if member.returns_result {
                quote!(#body_call.map_err(|e| e.into_native(#op))?)
            } else {
                body_call
            };
            let promise_wrap = if member.promise {
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
                let __out = ::otter_vm::marshal::IntoJs::into_js(__result, &mut __cx)
                    .map_err(|e| e.into_native(#op))?;
                #promise_wrap
                ::core::result::Result::Ok(__cx.escape(__out))
            }
        };
        glue.extend(quote! {
            fn #glue_ident(
                ctx: &mut ::otter_vm::NativeCtx<'_>,
                args: &[::otter_vm::Value],
            ) -> ::core::result::Result<::otter_vm::Value, ::otter_vm::NativeError> {
                ctx.scope(|ctx, __s| {
                    let mut __cx = ::otter_vm::marshal::MarshalCx::new(ctx, __s);
                    #(#extractions)*
                    #output
                })
            }
        });
        method_rows.push(quote!(#js_name / #length_lit => #glue_ident));
    }

    let intrinsic_ident = format_ident!("{type_ident}Intrinsic");
    let tag_field = match &args.tag {
        Some(tag) => quote!(string_tag = #tag,),
        None => quote!(),
    };
    // Members whose JS name starts with `__` are private compute hooks meant
    // only for the glue, not the public namespace. The macro wraps the glue in
    // a factory: it moves each private member off the namespace object into a
    // `natives` bag and calls the glue with it, so the glue reads
    // `natives.hmacSign` instead of poking `crypto.__hmacSign` and deleting it
    // by hand, and the public object never keeps a raw hook.
    let js_field = match &args.js {
        Some(path) => {
            let ns = ns_name.value();
            let mut collect = String::new();
            for member in &members {
                let name = member.js_name.value();
                if let Some(key) = name.strip_prefix("__") {
                    collect.push_str(&format!("natives.{key}=__ns.{name};delete __ns.{name};"));
                }
            }
            let prologue = format!(
                "(function(){{'use strict';var __ns=globalThis.{ns};\
                 var natives=Object.create(null);{collect}\
                 (function(natives){{'use strict';\n"
            );
            let prologue_lit = LitStr::new(&prologue, Span::call_site());
            let epilogue_lit = LitStr::new("\n})(natives);})();", Span::call_site());
            quote!(js_glue = ::core::concat!(#prologue_lit, include_str!(#path), #epilogue_lit),)
        }
        None => quote!(),
    };
    glue.extend(quote! {
        ::otter_macros::holt! {
            name = #ns_name,
            feature = #feature,
            intrinsic = #intrinsic_ident,
            methods = { #(#method_rows,)* },
            link_object_prototype = true,
            #tag_field
            #js_field
        }
    });
    Ok(glue)
}
