//! `#[js_class]` attribute macro — declarative host-class generator.
//!
//! The v2 class declaration form: an ordinary `impl` block whose
//! signatures are the descriptor. Parameter types declare argument
//! extraction (`FromJs`), return types declare result construction
//! (`IntoJs`), receivers declare brand checks, and marker attributes
//! (`#[constructor]` / `#[method]` / `#[getter]`) carry the explicit
//! JS names. Expansion emits per-member glue functions plus a
//! `couch!` invocation, so install, prototype assembly, and the
//! `Intrinsic` handle are exactly the proven `couch!` machinery.
//!
//! # Contents
//! - [`expand`] — entry called from the proc-macro shim.
//! - [`ClassArgs`] — parsed `#[js_class(...)]` arguments.
//! - [`Member`] — one classified impl-block member.
//!
//! # Surface
//!
//! ```rust,ignore
//! #[js_class(name = "Blob", feature = WEB)]
//! impl Blob {
//!     #[constructor]
//!     fn new(parts: Option<Sequence<BlobPart<'_>>>, options: Option<BlobPropertyBag>)
//!         -> Result<Blob, JsError> { … }
//!
//!     #[getter(name = "size")]
//!     fn size(&self) -> f64 { … }
//!
//!     #[method(name = "slice", length = 2)]
//!     fn slice(&self, start: Option<f64>, end: Option<f64>, ct: Option<USVString>) -> Blob { … }
//!
//!     #[method(name = "arrayBuffer", promise)]
//!     fn array_buffer(&self) -> Result<ArrayBuffer, JsError> { … }
//!
//!     #[method(name = "fastPath", length = 1, raw)]
//!     fn fast(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> { … }
//! }
//! ```
//!
//! Class-level arguments:
//! - `name = "Blob"` — required; the JS class name (never inferred).
//! - `feature = WEB` — required; forwarded to `couch!`.
//! - `extends = Base` — optional; native inheritance. Emits the
//!   `couch!` `parent`/`ctor_parent` resolvers via
//!   `<Base as HostClassMeta>::JS_NAME` (the base class must be
//!   installed first) and relies on the data struct's
//!   `#[derive(HostClass)]` ancestry for base-method dispatch.
//! - `tag = "…"` — optional `Symbol.toStringTag` override; defaults to
//!   `name` per WebIDL.
//!
//! Member attributes:
//! - `#[constructor]` (optional `length = N`) — exactly one, unless
//!   every member is `raw`. The body returns the *data* (`Self` or
//!   `Result<Self, JsError>`); the glue builds the instance with
//!   `new.target.prototype` linkage via `construct_instance`.
//! - `#[method(name = "…")]` — options `length = N`, `promise`
//!   (wrap the converted return in a pre-fulfilled promise), `raw`
//!   (the fn *is* the native entry: `fn(&mut NativeCtx, &[Value]) ->
//!   Result<Value, NativeError>`; no glue).
//! - `#[getter(name = "…")]` / `#[setter(name = "…")]` — prototype
//!   accessor halves; same-name halves merge into one accessor. A
//!   getter takes no parameters; a setter takes exactly one and its
//!   JS completion value is `undefined`.
//!
//! `length` defaults to the number of leading non-`Option` parameters
//! (the WebIDL rule); an explicit `length = N` wins. Fallible bodies
//! must spell the return type literally as `Result<T, JsError>` —
//! aliases that hide `Result` are not recognized.
//!
//! # Invariants
//! - JS names and export shape are explicit at the declaration site;
//!   the macro never derives a JS name from a Rust identifier.
//! - Generated glue touches JS values only through `MarshalCx` inside
//!   one `ctx.scope`, so it inherits the handle-scope rooting
//!   contract wholesale.
//! - The emitted `couch!` invocation is the single install path; this
//!   macro adds no runtime registration of its own.
//!
//! # See also
//! - [`crate::couch`](super::couch) — the spec/installer machinery
//!   this expands into.
//! - `EXTENSION_API_PLAN.md` (repo root) — the design.

use proc_macro::TokenStream;
use proc_macro2::Span;
use quote::{format_ident, quote};
use syn::parse::{Parse, ParseStream};
use syn::spanned::Spanned;
use syn::{
    Error, FnArg, Ident, ImplItem, ImplItemFn, ItemImpl, LitInt, LitStr, Path, Result, ReturnType,
    Token, Type,
};

/// Parsed `#[js_class(name = "…", feature = F, extends = P, tag = "…")]`.
pub(crate) struct ClassArgs {
    name: LitStr,
    feature: Ident,
    extends: Option<Path>,
    tag: Option<LitStr>,
    js: Option<LitStr>,
}

impl Parse for ClassArgs {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let mut name: Option<LitStr> = None;
        let mut feature: Option<Ident> = None;
        let mut extends: Option<Path> = None;
        let mut tag: Option<LitStr> = None;
        let mut js: Option<LitStr> = None;
        while !input.is_empty() {
            let key: Ident = input.parse()?;
            input.parse::<Token![=]>()?;
            match key.to_string().as_str() {
                "name" => name = Some(input.parse()?),
                "feature" => feature = Some(input.parse()?),
                "extends" => extends = Some(input.parse()?),
                "tag" => tag = Some(input.parse()?),
                "js" => js = Some(input.parse()?),
                other => {
                    return Err(Error::new(
                        key.span(),
                        format!(
                            "unknown js_class option `{other}`; expected \
                             `name`, `feature`, `extends`, `tag`, or `js`"
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
                .ok_or_else(|| Error::new(Span::call_site(), "js_class requires `name = \"…\"`"))?,
            feature: feature
                .ok_or_else(|| Error::new(Span::call_site(), "js_class requires `feature = …`"))?,
            extends,
            tag,
            js,
        })
    }
}

/// Receiver shape of a classified member.
enum ReceiverKind {
    Ref,
    RefMut,
    Owned,
    None,
}

/// Member role parsed from the marker attribute.
enum Role {
    Constructor {
        length: Option<u8>,
    },
    Method {
        js_name: LitStr,
        length: Option<u8>,
        promise: bool,
        raw: bool,
    },
    StaticMethod {
        js_name: LitStr,
        length: Option<u8>,
        promise: bool,
        raw: bool,
    },
    Getter {
        js_name: LitStr,
    },
    Setter {
        js_name: LitStr,
    },
}

/// One classified impl-block member.
struct Member {
    role: Role,
    fn_ident: Ident,
    receiver: ReceiverKind,
    /// Non-receiver parameter types, in order.
    params: Vec<Type>,
    /// Whether the return type is literally `Result<…>`.
    returns_result: bool,
    /// `async fn` — compiles to the promise protocol.
    is_async: bool,
}

/// Extract and strip the single marker attribute of `item`, if any.
fn classify(item: &mut ImplItemFn) -> Result<Option<Role>> {
    let mut role: Option<Role> = None;
    let mut keep = Vec::with_capacity(item.attrs.len());
    for attr in item.attrs.drain(..) {
        let ident = attr.path().get_ident().map(ToString::to_string);
        match ident.as_deref() {
            Some("constructor") => {
                let mut length: Option<u8> = None;
                if !matches!(attr.meta, syn::Meta::Path(_)) {
                    attr.parse_nested_meta(|meta| {
                        if meta.path.is_ident("length") {
                            let lit: LitInt = meta.value()?.parse()?;
                            length = Some(lit.base10_parse()?);
                            Ok(())
                        } else {
                            Err(meta.error("constructor supports only `length = N`"))
                        }
                    })?;
                }
                set_role(&mut role, Role::Constructor { length }, attr.span())?;
            }
            Some(kind @ ("method" | "static_method")) => {
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
                        return Err(meta.error(
                            "method supports `name = \"…\"`, `length = N`, `promise`, `raw`",
                        ));
                    }
                    Ok(())
                })?;
                let js_name = js_name.ok_or_else(|| {
                    Error::new(attr.span(), "method requires an explicit `name = \"…\"`")
                })?;
                let parsed = if kind == "method" {
                    Role::Method {
                        js_name,
                        length,
                        promise,
                        raw,
                    }
                } else {
                    Role::StaticMethod {
                        js_name,
                        length,
                        promise,
                        raw,
                    }
                };
                set_role(&mut role, parsed, attr.span())?;
            }
            Some(kind @ ("getter" | "setter")) => {
                let mut js_name: Option<LitStr> = None;
                attr.parse_nested_meta(|meta| {
                    if meta.path.is_ident("name") {
                        js_name = Some(meta.value()?.parse()?);
                        Ok(())
                    } else {
                        Err(meta.error("accessor markers support only `name = \"…\"`"))
                    }
                })?;
                let js_name = js_name.ok_or_else(|| {
                    Error::new(attr.span(), "accessor markers require `name = \"…\"`")
                })?;
                let parsed = if kind == "getter" {
                    Role::Getter { js_name }
                } else {
                    Role::Setter { js_name }
                };
                set_role(&mut role, parsed, attr.span())?;
            }
            _ => keep.push(attr),
        }
    }
    item.attrs = keep;
    Ok(role)
}

fn set_role(slot: &mut Option<Role>, role: Role, span: Span) -> Result<()> {
    if slot.is_some() {
        return Err(Error::new(span, "member has more than one js_class marker"));
    }
    *slot = Some(role);
    Ok(())
}

fn member_shape(item: &ImplItemFn, role: Role) -> Result<Member> {
    let is_async = item.sig.asyncness.is_some();
    let mut receiver = ReceiverKind::None;
    let mut params = Vec::new();
    for input in &item.sig.inputs {
        match input {
            FnArg::Receiver(recv) => {
                if recv.reference.is_none() {
                    if !is_async {
                        return Err(Error::new(
                            recv.span(),
                            "sync js_class members take `&self` / `&mut self`; \
                             an owned `self` receiver is the async-method shape",
                        ));
                    }
                    receiver = ReceiverKind::Owned;
                } else if is_async {
                    return Err(Error::new(
                        recv.span(),
                        "async js_class methods take an owned `self` snapshot \
                         (the future cannot borrow the instance across .await)",
                    ));
                } else {
                    receiver = if recv.mutability.is_some() {
                        ReceiverKind::RefMut
                    } else {
                        ReceiverKind::Ref
                    };
                }
            }
            FnArg::Typed(arg) => params.push((*arg.ty).clone()),
        }
    }
    let returns_result = match &item.sig.output {
        ReturnType::Default => false,
        ReturnType::Type(_, ty) => type_is_result(ty),
    };
    Ok(Member {
        role,
        fn_ident: item.sig.ident.clone(),
        receiver,
        params,
        returns_result,
        is_async,
    })
}

fn type_is_result(ty: &Type) -> bool {
    let Type::Path(path) = ty else { return false };
    path.path
        .segments
        .last()
        .is_some_and(|segment| segment.ident == "Result")
}

/// WebIDL `length`: leading parameters up to the first `Option<…>`.
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

/// Tokens that extract argument `index` of type `ty` into `__arg_{index}`.
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

fn arg_idents(count: usize) -> Vec<Ident> {
    (0..count).map(|i| format_ident!("__arg_{i}")).collect()
}

/// Expand `#[js_class]` — see the module docs for the surface.
pub(crate) fn expand(attr: TokenStream, item: TokenStream) -> TokenStream {
    let args = syn::parse_macro_input!(attr as ClassArgs);
    let mut class_impl = syn::parse_macro_input!(item as ItemImpl);
    match expand_inner(&args, &mut class_impl) {
        Ok(generated) => {
            let mut out = quote!(#class_impl);
            out.extend(generated);
            out.into()
        }
        Err(err) => err.to_compile_error().into(),
    }
}

fn expand_inner(args: &ClassArgs, class_impl: &mut ItemImpl) -> Result<proc_macro2::TokenStream> {
    if class_impl.trait_.is_some() {
        return Err(Error::new(
            class_impl.span(),
            "js_class goes on an inherent impl block, not a trait impl",
        ));
    }
    if !class_impl.generics.params.is_empty() {
        return Err(Error::new(
            class_impl.generics.span(),
            "js_class does not support generic host-data types",
        ));
    }
    let self_ty = (*class_impl.self_ty).clone();
    let Type::Path(self_path) = &self_ty else {
        return Err(Error::new(
            class_impl.self_ty.span(),
            "js_class requires a plain type name",
        ));
    };
    let type_ident = self_path
        .path
        .segments
        .last()
        .map(|segment| segment.ident.clone())
        .ok_or_else(|| Error::new(self_path.span(), "js_class requires a plain type name"))?;
    let glue_prefix = type_ident.to_string().to_lowercase();
    let class_name = &args.name;
    let feature = &args.feature;
    let tag = args.tag.clone().unwrap_or_else(|| class_name.clone());

    let mut members = Vec::new();
    for item in &mut class_impl.items {
        if let ImplItem::Fn(fn_item) = item
            && let Some(role) = classify(fn_item)?
        {
            members.push(member_shape(fn_item, role)?);
        }
    }

    let mut constructor: Option<&Member> = None;
    let mut methods: Vec<&Member> = Vec::new();
    let mut static_methods: Vec<&Member> = Vec::new();
    let mut getters: Vec<&Member> = Vec::new();
    let mut setters: Vec<&Member> = Vec::new();
    for member in &members {
        match &member.role {
            Role::Constructor { .. } => {
                if constructor.is_some() {
                    return Err(Error::new(
                        member.fn_ident.span(),
                        "js_class allows exactly one #[constructor]",
                    ));
                }
                constructor = Some(member);
            }
            Role::Method { .. } => methods.push(member),
            Role::StaticMethod { .. } => static_methods.push(member),
            Role::Getter { .. } => getters.push(member),
            Role::Setter { .. } => setters.push(member),
        }
    }
    let constructor = constructor.ok_or_else(|| {
        Error::new(
            class_impl.span(),
            "js_class requires exactly one #[constructor] member",
        )
    })?;

    let mut glue = proc_macro2::TokenStream::new();
    let mut method_rows = Vec::new();
    let mut static_rows = Vec::new();
    let mut accessor_rows = Vec::new();

    // Constructor glue.
    let ctor_glue_ident = format_ident!("__otter_js_class_{glue_prefix}_constructor");
    let ctor_length = match &constructor.role {
        Role::Constructor { length } => {
            length.unwrap_or_else(|| default_length(&constructor.params))
        }
        _ => unreachable!("filtered above"),
    };
    {
        if !matches!(constructor.receiver, ReceiverKind::None) {
            return Err(Error::new(
                constructor.fn_ident.span(),
                "#[constructor] takes no receiver; it returns the instance data",
            ));
        }
        if constructor.is_async {
            return Err(Error::new(
                constructor.fn_ident.span(),
                "constructors are synchronous; async factories are statics",
            ));
        }
        let op = class_name;
        let extractions: Vec<_> = constructor
            .params
            .iter()
            .enumerate()
            .map(|(index, ty)| arg_extraction(index, ty, op))
            .collect();
        let arg_names = arg_idents(constructor.params.len());
        let fn_ident = &constructor.fn_ident;
        let body_call = quote!(<#self_ty>::#fn_ident(#(#arg_names),*));
        let normalized = if constructor.returns_result {
            quote!(#body_call.map_err(|e| e.into_native(#op))?)
        } else {
            body_call
        };
        glue.extend(quote! {
            fn #ctor_glue_ident(
                ctx: &mut ::otter_vm::NativeCtx<'_>,
                args: &[::otter_vm::Value],
            ) -> ::core::result::Result<::otter_vm::Value, ::otter_vm::NativeError> {
                ctx.scope(|ctx, __s| {
                    let mut __cx = ::otter_vm::marshal::MarshalCx::new(ctx, __s);
                    #(#extractions)*
                    let __data = #normalized;
                    let __instance =
                        ::otter_vm::marshal::construct_instance(&mut __cx, #class_name, __data)
                            .map_err(|e| e.into_native(#op))?;
                    ::core::result::Result::Ok(__cx.escape(__instance))
                })
            }
        });
    }

    // Static methods: no receiver, own data properties on the
    // constructor (couch! `statics` rows). Same glue shape as
    // namespace members, including the async promise protocol.
    for member in &static_methods {
        let Role::StaticMethod {
            js_name,
            length,
            promise,
            raw,
        } = &member.role
        else {
            unreachable!("bucketed above");
        };
        let length = length.unwrap_or_else(|| default_length(&member.params));
        let length_lit = LitInt::new(&length.to_string(), Span::call_site());
        let fn_ident = &member.fn_ident;
        if !matches!(member.receiver, ReceiverKind::None) {
            return Err(Error::new(
                fn_ident.span(),
                "static methods take no receiver",
            ));
        }
        if *raw {
            let type_path = &self_path.path;
            static_rows.push(quote!(#js_name / #length_lit => #type_path::#fn_ident));
            continue;
        }
        if *promise && member.is_async {
            return Err(Error::new(
                fn_ident.span(),
                "async methods already return a promise; drop `promise`",
            ));
        }
        let op_string = format!("{}.{}", class_name.value(), js_name.value());
        let op = LitStr::new(&op_string, js_name.span());
        let glue_ident = format_ident!("__otter_js_class_{glue_prefix}_static_{fn_ident}");
        let extractions: Vec<_> = member
            .params
            .iter()
            .enumerate()
            .map(|(index, ty)| arg_extraction(index, ty, &op))
            .collect();
        let arg_names = arg_idents(member.params.len());
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
            let promise_wrap = if *promise {
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
        static_rows.push(quote!(#js_name / #length_lit => #glue_ident));
    }

    // Method + accessor glue. Getter/setter pairs merge into one
    // accessor row per JS name, ordered by first appearance.
    enum Emitted {
        Method,
        Getter,
        Setter,
    }
    let mut accessor_slots: Vec<(String, LitStr, Option<Ident>, Option<Ident>)> = Vec::new();
    for member in methods.iter().chain(getters.iter()).chain(setters.iter()) {
        let (js_name, length, promise, raw, kind) = match &member.role {
            Role::Method {
                js_name,
                length,
                promise,
                raw,
            } => (
                js_name.clone(),
                length.unwrap_or_else(|| default_length(&member.params)),
                *promise,
                *raw,
                Emitted::Method,
            ),
            Role::Getter { js_name } => (js_name.clone(), 0, false, false, Emitted::Getter),
            Role::Setter { js_name } => (js_name.clone(), 1, false, false, Emitted::Setter),
            Role::Constructor { .. } | Role::StaticMethod { .. } => {
                unreachable!("filtered above")
            }
        };
        let fn_ident = &member.fn_ident;

        if raw {
            if !matches!(kind, Emitted::Method) {
                return Err(Error::new(fn_ident.span(), "accessors cannot be `raw`"));
            }
            let name_lit = js_name;
            let length_lit = LitInt::new(&length.to_string(), Span::call_site());
            let type_path = &self_path.path;
            method_rows.push(quote!(#name_lit / #length_lit => #type_path::#fn_ident));
            continue;
        }

        if member.is_async {
            if !matches!(kind, Emitted::Method) {
                return Err(Error::new(fn_ident.span(), "only methods can be async"));
            }
            if promise {
                return Err(Error::new(
                    fn_ident.span(),
                    "async methods already return a promise; drop `promise`",
                ));
            }
            if !matches!(member.receiver, ReceiverKind::Owned) {
                return Err(Error::new(
                    fn_ident.span(),
                    "async methods take an owned `self` snapshot receiver",
                ));
            }
            let op_string = format!("{}.prototype.{}", class_name.value(), js_name.value());
            let op = LitStr::new(&op_string, js_name.span());
            let glue_ident = format_ident!("__otter_js_class_{glue_prefix}_{fn_ident}");
            let extractions: Vec<_> = member
                .params
                .iter()
                .enumerate()
                .map(|(index, ty)| arg_extraction(index, ty, &op))
                .collect();
            let arg_names = arg_idents(member.params.len());
            // The future's output must be Result<R, JsError>; a plain
            // return type wraps in Ok.
            let future_expr = if member.returns_result {
                quote!(__call)
            } else {
                quote!(async move {
                    ::core::result::Result::<_, ::otter_vm::marshal::JsError>::Ok(__call.await)
                })
            };
            glue.extend(quote! {
                fn #glue_ident(
                    ctx: &mut ::otter_vm::NativeCtx<'_>,
                    args: &[::otter_vm::Value],
                ) -> ::core::result::Result<::otter_vm::Value, ::otter_vm::NativeError> {
                    let __this_value = *ctx.this_value();
                    ctx.scope(|ctx, __s| {
                        let mut __cx = ::otter_vm::marshal::MarshalCx::new(ctx, __s);
                        let __this = __cx.park(__this_value);
                        // Owned snapshot: nothing GC-touching crosses
                        // the .await inside the future.
                        let __recv: #self_ty = __cx
                            .with_host_data::<#self_ty, #self_ty>(
                                __this,
                                ::core::clone::Clone::clone,
                            )
                            .map_err(|e| e.into_native(#op))?;
                        #(#extractions)*
                        let __call = <#self_ty>::#fn_ident(__recv #(, #arg_names)*);
                        let __future = #future_expr;
                        let __out = __cx
                            .promise_from_future(__future)
                            .map_err(|e| e.into_native(#op))?;
                        ::core::result::Result::Ok(__cx.escape(__out))
                    })
                }
            });
            let length_lit = LitInt::new(&length.to_string(), Span::call_site());
            method_rows.push(quote!(#js_name / #length_lit => #glue_ident));
            continue;
        }

        if matches!(kind, Emitted::Getter) && !member.params.is_empty() {
            return Err(Error::new(fn_ident.span(), "getters take no parameters"));
        }
        if matches!(kind, Emitted::Setter) && member.params.len() != 1 {
            return Err(Error::new(
                fn_ident.span(),
                "setters take exactly one parameter",
            ));
        }

        let op_string = format!("{}.prototype.{}", class_name.value(), js_name.value());
        let op = LitStr::new(&op_string, js_name.span());
        let glue_ident = format_ident!("__otter_js_class_{glue_prefix}_{fn_ident}");
        let extractions: Vec<_> = member
            .params
            .iter()
            .enumerate()
            .map(|(index, ty)| arg_extraction(index, ty, &op))
            .collect();
        let arg_names = arg_idents(member.params.len());

        let body_result = match member.receiver {
            ReceiverKind::Ref => quote! {
                __cx.with_host_data::<#self_ty, _>(__this, |__recv| {
                    __recv.#fn_ident(#(#arg_names),*)
                })
                .map_err(|e| e.into_native(#op))?
            },
            ReceiverKind::RefMut => quote! {
                __cx.with_host_data_mut::<#self_ty, _>(__this, |__recv| {
                    __recv.#fn_ident(#(#arg_names),*)
                })
                .map_err(|e| e.into_native(#op))?
            },
            ReceiverKind::Owned | ReceiverKind::None => {
                return Err(Error::new(
                    fn_ident.span(),
                    "non-raw sync methods take `&self` or `&mut self`; \
                     statics arrive with the namespace surface",
                ));
            }
        };
        let normalized = if member.returns_result {
            quote!(#body_result.map_err(|e| e.into_native(#op))?)
        } else {
            body_result
        };
        // A setter's completion value is `undefined`; everything else
        // converts the body result.
        let output = if matches!(kind, Emitted::Setter) {
            quote! {
                #normalized;
                ::core::result::Result::Ok(::otter_vm::Value::undefined())
            }
        } else {
            let promise_wrap = if promise {
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
                let __this_value = *ctx.this_value();
                ctx.scope(|ctx, __s| {
                    let mut __cx = ::otter_vm::marshal::MarshalCx::new(ctx, __s);
                    let __this = __cx.park(__this_value);
                    #(#extractions)*
                    #output
                })
            }
        });

        match kind {
            Emitted::Method => {
                let length_lit = LitInt::new(&length.to_string(), Span::call_site());
                method_rows.push(quote!(#js_name / #length_lit => #glue_ident));
            }
            Emitted::Getter | Emitted::Setter => {
                let key = js_name.value();
                let slot = match accessor_slots.iter_mut().find(|(name, ..)| *name == key) {
                    Some(slot) => slot,
                    None => {
                        accessor_slots.push((key, js_name.clone(), None, None));
                        accessor_slots.last_mut().expect("just pushed")
                    }
                };
                let target = if matches!(kind, Emitted::Getter) {
                    &mut slot.2
                } else {
                    &mut slot.3
                };
                if target.is_some() {
                    return Err(Error::new(
                        fn_ident.span(),
                        format!("duplicate accessor half for '{}'", js_name.value()),
                    ));
                }
                *target = Some(glue_ident);
            }
        }
    }
    for (_, js_name, get, set) in &accessor_slots {
        let row = match (get, set) {
            (Some(get), Some(set)) => quote!((#js_name, get = #get, set = #set)),
            (Some(get), None) => quote!((#js_name, get = #get)),
            (None, Some(set)) => quote!((#js_name, set = #set)),
            (None, None) => unreachable!("slot created with one half"),
        };
        accessor_rows.push(row);
    }

    // Marshalling impls: snapshot extraction, instance construction,
    // union distinguishability, class metadata.
    glue.extend(quote! {
        impl ::otter_vm::marshal::HostClassMeta for #self_ty {
            const JS_NAME: &'static str = #class_name;
        }

        impl ::otter_vm::marshal::IntoJs for #self_ty {
            fn into_js<'s>(
                self,
                cx: &mut ::otter_vm::marshal::MarshalCx<'_, '_, 's>,
            ) -> ::core::result::Result<
                ::otter_vm::marshal::JsValue<'s>,
                ::otter_vm::marshal::JsError,
            > {
                ::otter_vm::marshal::class_instance(cx, #class_name, self)
            }
        }

        impl<'s> ::otter_vm::marshal::FromJs<'s> for #self_ty {
            fn from_js(
                cx: &mut ::otter_vm::marshal::MarshalCx<'_, '_, 's>,
                v: ::otter_vm::marshal::JsValue<'s>,
                ident: ::otter_vm::marshal::ValueIdent<'_>,
            ) -> ::core::result::Result<Self, ::otter_vm::marshal::JsError> {
                let _ = ident;
                cx.with_host_data::<Self, Self>(v, ::core::clone::Clone::clone)
            }
        }

        impl ::otter_vm::marshal::JsUnionProbe for #self_ty {
            fn probe(
                cx: &::otter_vm::marshal::MarshalCx<'_, '_, '_>,
                v: ::otter_vm::marshal::JsValue<'_>,
            ) -> bool {
                cx.with_host_data::<Self, ()>(v, |_| ()).is_ok()
            }
        }
    });

    // Inheritance resolvers: the parent class installs first; its ctor
    // and prototype are read back off the global by JS name.
    let mut couch_extras = proc_macro2::TokenStream::new();
    let mut prototype_parent = proc_macro2::TokenStream::new();
    if let Some(parent) = &args.extends {
        let parent_proto_ident = format_ident!("__otter_js_class_{glue_prefix}_parent_proto");
        let parent_ctor_ident = format_ident!("__otter_js_class_{glue_prefix}_parent_ctor");
        glue.extend(quote! {
            fn #parent_proto_ident(
                global: ::otter_vm::JsObject,
                heap: &mut ::otter_gc::GcHeap,
            ) -> ::otter_vm::JsObject {
                let parent_name = <#parent as ::otter_vm::marshal::HostClassMeta>::JS_NAME;
                let ctor = ::otter_vm::object::get(global, heap, parent_name)
                    .and_then(|v| v.as_native_function())
                    .expect("js_class parent must be installed before the subclass");
                let desc = ctor
                    .own_property_descriptor(heap, "prototype")
                    .ok()
                    .flatten()
                    .expect("js_class parent prototype must exist");
                match desc.kind {
                    ::otter_vm::object::DescriptorKind::Data { value } => value
                        .as_object()
                        .expect("js_class parent prototype must be an object"),
                    _ => panic!("js_class parent prototype must be a data descriptor"),
                }
            }

            fn #parent_ctor_ident(
                global: ::otter_vm::JsObject,
                heap: &mut ::otter_gc::GcHeap,
            ) -> ::otter_vm::Value {
                let parent_name = <#parent as ::otter_vm::marshal::HostClassMeta>::JS_NAME;
                ::otter_vm::object::get(global, heap, parent_name)
                    .expect("js_class parent must be installed before the subclass")
            }
        });
        prototype_parent.extend(quote!(parent = #parent_proto_ident,));
        couch_extras.extend(quote!(ctor_parent = #parent_ctor_ident,));
    }

    let methods_block = if method_rows.is_empty() {
        quote!()
    } else {
        quote!(methods = { #(#method_rows,)* },)
    };
    let accessors_block = if accessor_rows.is_empty() {
        quote!()
    } else {
        quote!(accessors = [ #(#accessor_rows,)* ],)
    };
    let ctor_length_lit = LitInt::new(&ctor_length.to_string(), Span::call_site());
    let prototype_block =
        if method_rows.is_empty() && accessor_rows.is_empty() && prototype_parent.is_empty() {
            quote!()
        } else {
            quote! {
                prototype = {
                    #methods_block
                    #accessors_block
                    #prototype_parent
                },
            }
        };

    // Distinct intrinsic ident per class, so several `#[js_class]`
    // declarations coexist in one module (couch!'s default is the
    // shared name `Intrinsic`).
    let intrinsic_ident = format_ident!("{type_ident}Intrinsic");
    // `include_str!` resolves relative to the declaring file, so the
    // attached glue lives next to its class.
    let js_glue_field = match &args.js {
        Some(path) => quote!(js_glue = include_str!(#path),),
        None => quote!(),
    };
    let statics_block = if static_rows.is_empty() {
        quote!()
    } else {
        quote!(statics = { #(#static_rows,)* },)
    };
    glue.extend(quote! {
        ::otter_macros::couch! {
            name = #class_name,
            feature = #feature,
            intrinsic = #intrinsic_ident,
            constructor = (length = #ctor_length_lit, call = #ctor_glue_ident),
            #couch_extras
            #statics_block
            #prototype_block
            string_tag = #tag,
            #js_glue_field
        }
    });

    Ok(glue)
}
