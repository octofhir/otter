//! `couch!` proc macro — class intrinsic generator.
//!
//! See the crate-level docs and
//! [`docs/otter-macros-design.md`](../../../docs/otter-macros-design.md)
//! for the naming theme and the full surface. This module owns the
//! parsing + expansion for the `couch!` invocation only.
//!
//! # Contents
//! - [`expand`] — entry called from the proc-macro shim in
//!   [`crate::couch`](super) re-exported as `couch!`.
//! - [`CouchInput`] — parsed top-level fields.
//! - [`ConstructorSpecArgs`] — parsed `constructor = (length = N, call = path)`
//!   tuple.
//!
//! # Surface
//!
//! Minimum invocation:
//!
//! ```rust,ignore
//! couch! {
//!     name = "Proxy",
//!     feature = CORE,
//!     constructor = (length = 2, call = proxy_ctor_call),
//! }
//! ```
//!
//! Optional fields:
//!
//! - `statics = { "name" / length => path, ... }` — own properties
//!   on the constructor itself (`Proxy.revocable`).
//! - `spec = MY_SPEC,` — override the derived `<NAME>_SPEC` ident.
//! - `intrinsic = MyIntrinsic,` — override the default `Intrinsic`
//!   ident.
//!
//! # Generated symbols
//!
//! - `pub static <NAME>_SPEC: ::otter_vm::ConstructorSpec` — the
//!   raw constructor spec (constructor metadata + static methods +
//!   empty prototype methods slot).
//! - `pub struct <INTRINSIC>;` + `impl BuiltinIntrinsic for
//!   <INTRINSIC>` whose `install` body allocates the constructor
//!   via `bootstrap::native_constructor_static_with_value_roots`,
//!   pins each static as an own data property on the constructor,
//!   and binds it on `globalThis` through
//!   `bootstrap::define_global_value`.
//!
//! Prototype methods + accessors are intentionally **not** wired
//! in the 4.1a skeleton — that lands once the first `couch!`
//! consumer needs them and we agree on the install path. The
//! constructor's `.prototype` slot is still allocated (NativeFunction
//! constructors always carry one), it just receives no extra
//! methods until the field is added.
//!
//! # See also
//! - [`crate::holt`](super::holt) — namespace intrinsic generator
//!   sharing the [`super::holt::MethodEntry`] grammar.

use proc_macro::TokenStream;
use proc_macro2::Span;
use quote::{format_ident, quote};
use std::collections::BTreeSet;
use syn::parse::{Parse, ParseStream};
use syn::{
    Ident, LitBool, LitInt, LitStr, Path, Result, Token, braced, bracketed, parenthesized,
    parse_macro_input,
};

use crate::holt::{AccessorEntry, MethodEntry};

/// Parsed `constructor = (length = N, call = path [, abstract = true])`
/// tuple.
pub(crate) struct ConstructorSpecArgs {
    pub(crate) length: u8,
    pub(crate) call: Path,
    pub(crate) is_abstract: bool,
}

impl Parse for ConstructorSpecArgs {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let body;
        parenthesized!(body in input);
        let mut length: Option<u8> = None;
        let mut call: Option<Path> = None;
        let mut is_abstract = false;
        while !body.is_empty() {
            let key: Ident = body.parse()?;
            body.parse::<Token![=]>()?;
            match key.to_string().as_str() {
                "length" => {
                    let lit: LitInt = body.parse()?;
                    length = Some(lit.base10_parse()?);
                }
                "call" => {
                    call = Some(body.parse()?);
                }
                "is_abstract" => {
                    let lit: LitBool = body.parse()?;
                    is_abstract = lit.value;
                }
                other => {
                    return Err(syn::Error::new(
                        key.span(),
                        format!(
                            "couch!: unknown constructor field `{other}` — expected \
                             `length`, `call`, or `is_abstract` (Rust reserves the \
                             bare `abstract` keyword)"
                        ),
                    ));
                }
            }
            if body.peek(Token![,]) {
                body.parse::<Token![,]>()?;
            }
        }
        Ok(Self {
            length: length.ok_or_else(|| {
                syn::Error::new(
                    Span::call_site(),
                    "couch!: constructor block missing `length = N`",
                )
            })?,
            call: call.ok_or_else(|| {
                syn::Error::new(
                    Span::call_site(),
                    "couch!: constructor block missing `call = path::to::fn`",
                )
            })?,
            is_abstract,
        })
    }
}

/// Parsed `prototype = { methods = { ... }, accessors = [...],
/// method_specs = [path, ...] }` block.
///
/// `methods` and `accessors` parse inline rows; `method_specs`
/// references pre-built `&[MethodSpec]` slices (used when a
/// `BuiltinIntrinsic` has many prototype methods declared through a
/// generator macro that emits one `pub static FOO: &[MethodSpec] =
/// &[...]` slice). The install body iterates each slice in order.
#[derive(Default)]
pub(crate) struct PrototypeBlock {
    pub(crate) methods: Vec<MethodEntry>,
    pub(crate) accessors: Vec<AccessorEntry>,
    pub(crate) method_specs: Vec<Path>,
}

impl Parse for PrototypeBlock {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let body;
        braced!(body in input);
        let mut methods: Vec<MethodEntry> = Vec::new();
        let mut accessors: Vec<AccessorEntry> = Vec::new();
        let mut method_specs: Vec<Path> = Vec::new();
        while !body.is_empty() {
            let key: Ident = body.parse()?;
            body.parse::<Token![=]>()?;
            match key.to_string().as_str() {
                "methods" => {
                    let methods_body;
                    braced!(methods_body in body);
                    while !methods_body.is_empty() {
                        methods.push(methods_body.parse()?);
                        if methods_body.peek(Token![,]) {
                            methods_body.parse::<Token![,]>()?;
                        }
                    }
                }
                "accessors" => {
                    let accessors_body;
                    bracketed!(accessors_body in body);
                    while !accessors_body.is_empty() {
                        accessors.push(accessors_body.parse()?);
                        if accessors_body.peek(Token![,]) {
                            accessors_body.parse::<Token![,]>()?;
                        }
                    }
                }
                "method_specs" => {
                    let specs_body;
                    bracketed!(specs_body in body);
                    while !specs_body.is_empty() {
                        method_specs.push(specs_body.parse()?);
                        if specs_body.peek(Token![,]) {
                            specs_body.parse::<Token![,]>()?;
                        }
                    }
                }
                other => {
                    return Err(syn::Error::new(
                        key.span(),
                        format!(
                            "couch! prototype: unknown field `{other}` — expected \
                             `methods`, `accessors`, or `method_specs`"
                        ),
                    ));
                }
            }
            if body.peek(Token![,]) {
                body.parse::<Token![,]>()?;
            }
        }
        Ok(Self {
            methods,
            accessors,
            method_specs,
        })
    }
}

/// Parsed body of a `couch!` invocation.
pub(crate) struct CouchInput {
    pub(crate) name: LitStr,
    pub(crate) feature: Ident,
    pub(crate) spec_ident: Ident,
    pub(crate) intrinsic_ident: Ident,
    pub(crate) constructor: ConstructorSpecArgs,
    pub(crate) statics: Vec<MethodEntry>,
    pub(crate) prototype: PrototypeBlock,
}

impl Parse for CouchInput {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let mut name: Option<LitStr> = None;
        let mut feature: Option<Ident> = None;
        let mut spec_override: Option<Ident> = None;
        let mut intrinsic_override: Option<Ident> = None;
        let mut constructor: Option<ConstructorSpecArgs> = None;
        let mut statics: Vec<MethodEntry> = Vec::new();
        let mut prototype: PrototypeBlock = PrototypeBlock::default();

        while !input.is_empty() {
            let key: Ident = input.parse()?;
            input.parse::<Token![=]>()?;
            match key.to_string().as_str() {
                "name" => name = Some(input.parse()?),
                "feature" => feature = Some(input.parse()?),
                "spec" => spec_override = Some(input.parse()?),
                "intrinsic" => intrinsic_override = Some(input.parse()?),
                "constructor" => constructor = Some(input.parse()?),
                "statics" => {
                    let body;
                    braced!(body in input);
                    while !body.is_empty() {
                        statics.push(body.parse()?);
                        if body.peek(Token![,]) {
                            body.parse::<Token![,]>()?;
                        }
                    }
                }
                "prototype" => {
                    prototype = input.parse()?;
                }
                other => {
                    return Err(syn::Error::new(
                        key.span(),
                        format!(
                            "unknown `couch!` field `{other}` — expected `name`, `feature`, \
                             `spec`, `intrinsic`, `constructor`, `statics`, or `prototype`"
                        ),
                    ));
                }
            }
            if input.peek(Token![,]) {
                input.parse::<Token![,]>()?;
            }
        }

        let name = name.ok_or_else(|| {
            syn::Error::new(Span::call_site(), "couch!: missing `name = \"...\"`")
        })?;
        let feature = feature.ok_or_else(|| {
            syn::Error::new(
                Span::call_site(),
                "couch!: missing `feature = <BootstrapFeatures variant>` (e.g. `feature = CORE`)",
            )
        })?;
        let constructor = constructor.ok_or_else(|| {
            syn::Error::new(
                Span::call_site(),
                "couch!: missing `constructor = (length = N, call = path::to::fn)`",
            )
        })?;

        let spec_ident = spec_override.unwrap_or_else(|| {
            let upper = name.value().to_ascii_uppercase();
            let sanitized: String = upper
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
                .collect();
            format_ident!("{}_SPEC", sanitized, span = name.span())
        });
        let intrinsic_ident =
            intrinsic_override.unwrap_or_else(|| Ident::new("Intrinsic", Span::call_site()));

        Ok(Self {
            name,
            feature,
            spec_ident,
            intrinsic_ident,
            constructor,
            statics,
            prototype,
        })
    }
}

pub(crate) fn expand(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as CouchInput);
    let CouchInput {
        name,
        feature,
        spec_ident,
        intrinsic_ident,
        constructor,
        statics,
        prototype,
    } = input;

    let mut seen = BTreeSet::new();
    for m in &statics {
        if !seen.insert(m.js_name.value()) {
            return syn::Error::new_spanned(
                &m.js_name,
                format!("couch!: duplicate static name `{}`", m.js_name.value()),
            )
            .to_compile_error()
            .into();
        }
    }
    let mut seen_proto_m = BTreeSet::new();
    for m in &prototype.methods {
        if !seen_proto_m.insert(m.js_name.value()) {
            return syn::Error::new_spanned(
                &m.js_name,
                format!("couch!: duplicate prototype method `{}`", m.js_name.value()),
            )
            .to_compile_error()
            .into();
        }
    }
    let mut seen_proto_a = BTreeSet::new();
    for a in &prototype.accessors {
        if !seen_proto_a.insert(a.js_name.value()) {
            return syn::Error::new_spanned(
                &a.js_name,
                format!(
                    "couch!: duplicate prototype accessor `{}`",
                    a.js_name.value()
                ),
            )
            .to_compile_error()
            .into();
        }
    }

    let statics_ident = format_ident!("__OTTER_{}_STATICS", spec_ident);
    let prototype_methods_ident = format_ident!("__OTTER_{}_PROTOTYPE_METHODS", spec_ident);
    let prototype_accessors_ident = format_ident!("__OTTER_{}_PROTOTYPE_ACCESSORS", spec_ident);
    let static_entries = statics.iter().map(|m| {
        let js_name = &m.js_name;
        let length = m.length;
        let call = &m.call;
        let attrs_path = crate::holt::attrs_factory_path(m.attrs.as_ref(), "builtin_function");
        quote! {
            ::otter_vm::MethodSpec {
                name: #js_name,
                length: #length,
                attrs: #attrs_path,
                call: ::otter_vm::NativeCall::Static(#call),
            }
        }
    });
    let prototype_method_entries = prototype.methods.iter().map(|m| {
        let js_name = &m.js_name;
        let length = m.length;
        let call = &m.call;
        let attrs_path = crate::holt::attrs_factory_path(m.attrs.as_ref(), "builtin_function");
        quote! {
            ::otter_vm::MethodSpec {
                name: #js_name,
                length: #length,
                attrs: #attrs_path,
                call: ::otter_vm::NativeCall::Static(#call),
            }
        }
    });
    let prototype_accessor_entries = prototype.accessors.iter().map(|a| {
        let js_name = &a.js_name;
        let get_tokens = match &a.get {
            Some(path) => quote! {
                ::core::option::Option::Some(::otter_vm::NativeCall::Static(#path))
            },
            None => quote! { ::core::option::Option::None },
        };
        let set_tokens = match &a.set {
            Some(path) => quote! {
                ::core::option::Option::Some(::otter_vm::NativeCall::Static(#path))
            },
            None => quote! { ::core::option::Option::None },
        };
        let attrs_path = crate::holt::attrs_factory_path(a.attrs.as_ref(), "builtin_function");
        quote! {
            ::otter_vm::AccessorSpec {
                name: #js_name,
                get: #get_tokens,
                set: #set_tokens,
                attrs: #attrs_path,
            }
        }
    });

    let ctor_length = constructor.length;
    let ctor_call = &constructor.call;
    let feature_path = quote! { ::otter_vm::bootstrap::BootstrapFeatures::#feature };
    // Abstract ctors still wire their `call` field (for diagnostics
    // + name resolution), but the macro emits the same install
    // path; the user's call body is expected to throw a TypeError.
    // The flag mostly documents intent today; future expansions may
    // synthesise a default throw stub when `abstract = true` and no
    // `call` is supplied.
    let _ = constructor.is_abstract;

    let prototype_has_method_specs = !prototype.method_specs.is_empty();
    let extra_method_spec_iters = prototype.method_specs.iter().map(|path| {
        quote! {
            for method_spec in #path.iter() {
                builder.method_from_spec(method_spec)?;
            }
        }
    });
    // Macro-time flag for whether the prototype block contributes
    // anything; controls the wrapping `if` in the install body.
    let prototype_block_needed = !prototype.methods.is_empty()
        || !prototype.accessors.is_empty()
        || prototype_has_method_specs;

    quote! {
        #[allow(non_upper_case_globals)]
        static #statics_ident: &[::otter_vm::MethodSpec] = &[
            #(#static_entries),*
        ];

        #[allow(non_upper_case_globals)]
        static #prototype_methods_ident: &[::otter_vm::MethodSpec] = &[
            #(#prototype_method_entries),*
        ];

        #[allow(non_upper_case_globals)]
        static #prototype_accessors_ident: &[::otter_vm::AccessorSpec] = &[
            #(#prototype_accessor_entries),*
        ];

        #[doc = "Generated constructor spec (see `couch!`)."]
        #[allow(non_upper_case_globals)]
        pub static #spec_ident: ::otter_vm::ConstructorSpec = ::otter_vm::ConstructorSpec {
            name: #name,
            length: #ctor_length,
            call: ::otter_vm::NativeCall::Static(#ctor_call),
            static_methods: #statics_ident,
            prototype_methods: #prototype_methods_ident,
            attrs: ::otter_vm::Attr::global_binding(),
        };

        #[doc = "Generated `BuiltinIntrinsic` adapter (see `couch!`)."]
        pub struct #intrinsic_ident;

        impl ::otter_vm::intrinsic_install::BuiltinIntrinsic for #intrinsic_ident {
            const NAME: &'static str = #name;
            const FEATURE: ::otter_vm::bootstrap::BootstrapFeatures = #feature_path;

            fn install(
                heap: &mut ::otter_gc::GcHeap,
                global: ::otter_vm::JsObject,
            ) -> ::core::result::Result<(), ::otter_vm::JsSurfaceError> {
                let global_root = ::otter_vm::Value::object(global);

                // Generated specs only ever carry `NativeCall::Static`;
                // every other variant is unreachable inside macro
                // expansion. Pattern out explicitly to keep
                // `cargo clippy --deny warnings` happy.
                let ctor_call = match #spec_ident.call {
                    ::otter_vm::NativeCall::Static(f) => f,
                    ::otter_vm::NativeCall::VmIntrinsic(_)
                    | ::otter_vm::NativeCall::Dynamic(_) => {
                        return ::core::result::Result::Err(
                            ::otter_vm::JsSurfaceError::DefinePropertyFailed(
                                "couch!: non-Static NativeCall in constructor spec",
                            ),
                        );
                    }
                };
                let ctor = ::otter_vm::bootstrap::native_constructor_static_with_value_roots(
                    heap,
                    #spec_ident.name,
                    #spec_ident.length,
                    ctor_call,
                    &[&global_root],
                )
                .map_err(::otter_vm::JsSurfaceError::from)?;
                let ctor_value = ::otter_vm::Value::native_function(ctor);

                for method_spec in #spec_ident.static_methods.iter() {
                    let call_target = match method_spec.call {
                        ::otter_vm::NativeCall::Static(f) => f,
                        ::otter_vm::NativeCall::VmIntrinsic(_)
                        | ::otter_vm::NativeCall::Dynamic(_) => {
                            return ::core::result::Result::Err(
                                ::otter_vm::JsSurfaceError::DefinePropertyFailed(method_spec.name),
                            );
                        }
                    };
                    let fn_obj = ::otter_vm::bootstrap::native_static_with_value_roots(
                        heap,
                        method_spec.name,
                        method_spec.length,
                        call_target,
                        &[&global_root, &ctor_value],
                    )
                    .map_err(::otter_vm::JsSurfaceError::from)?;
                    let desc = ::otter_vm::object::PropertyDescriptor::data(
                        ::otter_vm::Value::native_function(fn_obj),
                        method_spec.attrs.writable,
                        method_spec.attrs.enumerable,
                        method_spec.attrs.configurable,
                    );
                    if !ctor.define_own_property(heap, method_spec.name, desc) {
                        return ::core::result::Result::Err(
                            ::otter_vm::JsSurfaceError::DefinePropertyFailed(method_spec.name),
                        );
                    }
                }

                // §19.4 prototype object (only when the spec lists
                // prototype methods or accessors). Alloc empty
                // prototype + link to %Object.prototype% + pin each
                // entry via ObjectBuilder, then attach the prototype
                // back on the constructor as a non-writable /
                // non-enumerable / non-configurable own data
                // property (matches the canonical builtin prototype
                // descriptor).
                if #prototype_block_needed {
                    let prototype = ::otter_vm::bootstrap::alloc_object_with_value_roots_pub(
                        heap,
                        &[&global_root, &ctor_value],
                    )
                    .map_err(::otter_vm::JsSurfaceError::from)?;
                    if let ::core::option::Option::Some(object_ctor) =
                        ::otter_vm::object::get(global, heap, "Object")
                            .and_then(|v| v.as_object())
                        && let ::core::option::Option::Some(object_proto) =
                            ::otter_vm::object::get(object_ctor, heap, "prototype")
                                .and_then(|v| v.as_object())
                    {
                        ::otter_vm::object::set_prototype(
                            prototype,
                            heap,
                            ::core::option::Option::Some(object_proto),
                        );
                    }
                    let prototype_value = ::otter_vm::Value::object(prototype);
                    {
                        let mut builder =
                            ::otter_vm::ObjectBuilder::from_object_with_value_roots(
                                heap,
                                prototype,
                                ::std::vec![global_root, ctor_value, prototype_value],
                            );
                        for method_spec in #spec_ident.prototype_methods.iter() {
                            builder.method_from_spec(method_spec)?;
                        }
                        for accessor_spec in #prototype_accessors_ident.iter() {
                            builder.accessor_from_spec(accessor_spec)?;
                        }
                        // Extra `method_specs = [path, ...]` paths —
                        // iterate each pre-built `&[MethodSpec]` slice
                        // through the same `ObjectBuilder`. Used by
                        // builtins (e.g. `Date`) whose prototype
                        // method list is generated by a separate
                        // declarative macro that produces a static
                        // slice.
                        #(#extra_method_spec_iters)*
                    }
                    let proto_desc = ::otter_vm::object::PropertyDescriptor::data(
                        prototype_value,
                        false,
                        false,
                        false,
                    );
                    if !ctor.define_own_property(heap, "prototype", proto_desc) {
                        return ::core::result::Result::Err(
                            ::otter_vm::JsSurfaceError::DefinePropertyFailed("prototype"),
                        );
                    }
                    // §19.4 — every builtin constructor whose
                    // prototype carries methods/accessors gets a
                    // `prototype.constructor = ctor` back-pointer
                    // (writable / non-enumerable / configurable per
                    // spec).
                    let ctor_back_desc = ::otter_vm::object::PropertyDescriptor::data(
                        ctor_value,
                        true,
                        false,
                        true,
                    );
                    let _ = ::otter_vm::object::define_own_property(
                        prototype,
                        heap,
                        "constructor",
                        ctor_back_desc,
                    );
                }

                ::otter_vm::bootstrap::define_global_value(
                    global,
                    heap,
                    <Self as ::otter_vm::intrinsic_install::BuiltinIntrinsic>::NAME,
                    ctor_value,
                );
                ::core::result::Result::Ok(())
            }
        }
    }
    .into()
}
