//! Zero-cost JavaScript surface macros for Otter.
//!
//! These macros generate task-96 static specs and ordinary Rust
//! functions. They do not register globals, allocate at runtime, or
//! create dynamic dispatch paths for static builtins.
//!
//! # Contents
//! - [`js_namespace`] — attribute macro for namespace objects.
//! - [`js_class`] — attribute macro for constructor/prototype class
//!   specs.
//! - [`raft`] — grouped static namespace spec declaration macro.
//! - `#[js_fn(...)]` — helper attribute consumed inside
//!   [`js_namespace`].
//! - `#[js_constructor(...)]`, `#[js_method(...)]`,
//!   `#[js_static_method(...)]`, `#[js_getter(...)]`, and
//!   `#[js_setter(...)]` — helper attributes consumed inside
//!   [`js_class`].
//!
//! # Invariants
//! - Exported JavaScript names and arity are explicit in macro
//!   metadata.
//! - Expansion emits `NamespaceSpec`, `ClassSpec`, `ConstructorSpec`,
//!   and `MethodSpec` static data with `NativeCall::Static` function
//!   pointers.
//! - Bootstrap remains explicit; generated specs are installed by
//!   task-96 builders or the centralized bootstrap registry.
//!
//! # See also
//! - [`docs/new-engine/tasks/97-zero-cost-js-surface-macros.md`](
//!     ../../../docs/new-engine/tasks/97-zero-cost-js-surface-macros.md
//!   )

use std::collections::{BTreeMap, BTreeSet};

use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::parse::{Parse, ParseStream};
use syn::{
    Attribute, Ident, Item, ItemMod, LitInt, LitStr, Meta, Path, Result, Token, Visibility, braced,
    bracketed, parenthesized, parse_macro_input,
};

/// Generate a static `NamespaceSpec` from a Rust module.
///
/// The module must declare an explicit JavaScript name and spec
/// identifier:
///
/// ```rust,ignore
/// #[otter_macros::js_namespace(name = "Math", spec = MATH_SPEC)]
/// mod math {
///     #[js_fn(name = "abs", length = 1)]
///     pub fn abs(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
///         todo!()
///     }
/// }
/// ```
///
/// Expansion keeps the module intact, removes the consumed `js_fn`
/// attributes, and emits a static spec equivalent to handwritten
/// task-96 data.
#[proc_macro_attribute]
pub fn js_namespace(attr: TokenStream, item: TokenStream) -> TokenStream {
    let args = parse_macro_input!(attr as NamespaceArgs);
    let mut module = parse_macro_input!(item as ItemMod);

    let Some((_, items)) = module.content.as_mut() else {
        return syn::Error::new_spanned(
            &module.ident,
            "js_namespace requires an inline module body",
        )
        .to_compile_error()
        .into();
    };

    let mut methods = Vec::new();
    let mut seen = BTreeSet::new();
    for item in items.iter_mut() {
        let Item::Fn(func) = item else {
            continue;
        };
        match take_js_fn(&mut func.attrs) {
            Ok(Some(spec)) => {
                if !seen.insert(spec.name.value()) {
                    return syn::Error::new_spanned(
                        &func.sig.ident,
                        format!("duplicate js_fn name `{}`", spec.name.value()),
                    )
                    .to_compile_error()
                    .into();
                }
                methods.push(MethodBinding {
                    rust_name: func.sig.ident.clone(),
                    js_name: spec.name,
                    length: spec.length,
                });
            }
            Ok(None) => {}
            Err(err) => return err.to_compile_error().into(),
        }
    }

    let spec_ident = args.spec;
    let namespace_name = args.name;
    let methods_ident = format_ident!("__OTTER_{}_METHODS", spec_ident);
    let mod_ident = module.ident.clone();
    let method_entries = methods.iter().map(|method| {
        let rust_name = &method.rust_name;
        let js_name = &method.js_name;
        let length = &method.length;
        quote! {
            ::otter_vm::MethodSpec {
                name: #js_name,
                length: #length,
                attrs: ::otter_vm::Attr::builtin_function(),
                call: ::otter_vm::NativeCall::Static(#mod_ident::#rust_name),
            }
        }
    });

    quote! {
        #module

        #[allow(non_upper_case_globals)]
        static #methods_ident: &[::otter_vm::MethodSpec] = &[
            #(#method_entries),*
        ];

        #[allow(non_upper_case_globals)]
        #[doc = "Generated static JavaScript namespace spec."]
        pub static #spec_ident: ::otter_vm::NamespaceSpec = ::otter_vm::NamespaceSpec {
            name: #namespace_name,
            methods: #methods_ident,
            accessors: &[],
            constants: &[],
            attrs: ::otter_vm::Attr::global_binding(),
        };
    }
    .into()
}

/// Generate a static `ClassSpec` from a Rust module.
///
/// The module must contain exactly one `#[js_constructor(length = N)]`
/// function. Prototype instance methods use `#[js_method(...)]`;
/// constructor/static-side methods use `#[js_static_method(...)]`.
/// Prototype accessors use `#[js_getter(...)]` and
/// `#[js_setter(...)]`.
///
/// ```rust,ignore
/// #[otter_macros::js_class(name = "Point", spec = POINT_SPEC)]
/// mod point {
///     #[js_constructor(length = 1)]
///     pub fn construct(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
///         todo!()
///     }
///
///     #[js_method(name = "valueOf", length = 0)]
///     pub fn value_of(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
///         todo!()
///     }
/// }
/// ```
///
/// Expansion keeps the module intact, removes consumed helper
/// attributes, and emits a static class spec over the task-96 builder
/// backend.
#[proc_macro_attribute]
pub fn js_class(attr: TokenStream, item: TokenStream) -> TokenStream {
    let args = parse_macro_input!(attr as ClassArgs);
    let mut module = parse_macro_input!(item as ItemMod);

    let Some((_, items)) = module.content.as_mut() else {
        return syn::Error::new_spanned(&module.ident, "js_class requires an inline module body")
            .to_compile_error()
            .into();
    };

    let mut constructor = None;
    let mut static_methods = Vec::new();
    let mut prototype_methods = Vec::new();
    let mut prototype_accessors = BTreeMap::<String, AccessorBinding>::new();
    let mut seen_static = BTreeSet::new();
    let mut seen_prototype = BTreeSet::new();

    for item in items.iter_mut() {
        let Item::Fn(func) = item else {
            continue;
        };

        let ctor = match take_length_attr(&mut func.attrs, "js_constructor") {
            Ok(value) => value,
            Err(err) => return err.to_compile_error().into(),
        };
        let static_fn = match take_fn_attr(&mut func.attrs, "js_static_method") {
            Ok(value) => value,
            Err(err) => return err.to_compile_error().into(),
        };
        let proto_fn = match take_fn_attr(&mut func.attrs, "js_method") {
            Ok(value) => value,
            Err(err) => return err.to_compile_error().into(),
        };
        let getter = match take_name_attr(&mut func.attrs, "js_getter") {
            Ok(value) => value,
            Err(err) => return err.to_compile_error().into(),
        };
        let setter = match take_name_attr(&mut func.attrs, "js_setter") {
            Ok(value) => value,
            Err(err) => return err.to_compile_error().into(),
        };

        let helper_count = usize::from(ctor.is_some())
            + usize::from(static_fn.is_some())
            + usize::from(proto_fn.is_some())
            + usize::from(getter.is_some())
            + usize::from(setter.is_some());
        if helper_count > 1 {
            return syn::Error::new_spanned(
                &func.sig.ident,
                "only one JS class helper attribute is allowed per function",
            )
            .to_compile_error()
            .into();
        }

        if let Some(ctor) = ctor {
            if constructor.is_some() {
                return syn::Error::new_spanned(
                    &func.sig.ident,
                    "duplicate js_constructor in one js_class module",
                )
                .to_compile_error()
                .into();
            }
            constructor = Some(ConstructorBinding {
                rust_name: func.sig.ident.clone(),
                length: ctor.length,
            });
        }

        if let Some(spec) = static_fn {
            if !seen_static.insert(spec.name.value()) {
                return syn::Error::new_spanned(
                    &func.sig.ident,
                    format!("duplicate js_static_method name `{}`", spec.name.value()),
                )
                .to_compile_error()
                .into();
            }
            static_methods.push(MethodBinding {
                rust_name: func.sig.ident.clone(),
                js_name: spec.name,
                length: spec.length,
            });
        }

        if let Some(spec) = proto_fn {
            if prototype_accessors.contains_key(&spec.name.value()) {
                return syn::Error::new_spanned(
                    &func.sig.ident,
                    format!("js_method conflicts with accessor `{}`", spec.name.value()),
                )
                .to_compile_error()
                .into();
            }
            if !seen_prototype.insert(spec.name.value()) {
                return syn::Error::new_spanned(
                    &func.sig.ident,
                    format!("duplicate js_method name `{}`", spec.name.value()),
                )
                .to_compile_error()
                .into();
            }
            prototype_methods.push(MethodBinding {
                rust_name: func.sig.ident.clone(),
                js_name: spec.name,
                length: spec.length,
            });
        }

        if let Some(spec) = getter {
            if seen_prototype.contains(&spec.name.value()) {
                return syn::Error::new_spanned(
                    &func.sig.ident,
                    format!("js_getter conflicts with js_method `{}`", spec.name.value()),
                )
                .to_compile_error()
                .into();
            }
            let key = spec.name.value();
            let accessor = prototype_accessors
                .entry(key)
                .or_insert_with(|| AccessorBinding::new(spec.name));
            if accessor.get.is_some() {
                return syn::Error::new_spanned(
                    &func.sig.ident,
                    format!("duplicate js_getter name `{}`", accessor.js_name.value()),
                )
                .to_compile_error()
                .into();
            }
            accessor.get = Some(func.sig.ident.clone());
        }

        if let Some(spec) = setter {
            if seen_prototype.contains(&spec.name.value()) {
                return syn::Error::new_spanned(
                    &func.sig.ident,
                    format!("js_setter conflicts with js_method `{}`", spec.name.value()),
                )
                .to_compile_error()
                .into();
            }
            let key = spec.name.value();
            let accessor = prototype_accessors
                .entry(key)
                .or_insert_with(|| AccessorBinding::new(spec.name));
            if accessor.set.is_some() {
                return syn::Error::new_spanned(
                    &func.sig.ident,
                    format!("duplicate js_setter name `{}`", accessor.js_name.value()),
                )
                .to_compile_error()
                .into();
            }
            accessor.set = Some(func.sig.ident.clone());
        }
    }

    let Some(constructor) = constructor else {
        return syn::Error::new_spanned(
            &module.ident,
            "js_class requires one #[js_constructor(length = N)] function",
        )
        .to_compile_error()
        .into();
    };

    let spec_ident = args.spec;
    let class_name = args.name;
    let mod_ident = module.ident.clone();
    let static_methods_ident = format_ident!("__OTTER_{}_STATIC_METHODS", spec_ident);
    let prototype_methods_ident = format_ident!("__OTTER_{}_PROTOTYPE_METHODS", spec_ident);
    let prototype_accessors_ident = format_ident!("__OTTER_{}_PROTOTYPE_ACCESSORS", spec_ident);
    let constructor_rust_name = constructor.rust_name;
    let constructor_length = constructor.length;
    let static_method_entries = static_methods.iter().map(|method| {
        let rust_name = &method.rust_name;
        let js_name = &method.js_name;
        let length = &method.length;
        quote! {
            ::otter_vm::MethodSpec {
                name: #js_name,
                length: #length,
                attrs: ::otter_vm::Attr::builtin_function(),
                call: ::otter_vm::NativeCall::Static(#mod_ident::#rust_name),
            }
        }
    });
    let prototype_method_entries = prototype_methods.iter().map(|method| {
        let rust_name = &method.rust_name;
        let js_name = &method.js_name;
        let length = &method.length;
        quote! {
            ::otter_vm::MethodSpec {
                name: #js_name,
                length: #length,
                attrs: ::otter_vm::Attr::builtin_function(),
                call: ::otter_vm::NativeCall::Static(#mod_ident::#rust_name),
            }
        }
    });
    let prototype_accessor_entries = prototype_accessors.values().map(|accessor| {
        let js_name = &accessor.js_name;
        let get = accessor.get.as_ref().map_or_else(
            || quote! { None },
            |rust_name| quote! { Some(::otter_vm::NativeCall::Static(#mod_ident::#rust_name)) },
        );
        let set = accessor.set.as_ref().map_or_else(
            || quote! { None },
            |rust_name| quote! { Some(::otter_vm::NativeCall::Static(#mod_ident::#rust_name)) },
        );
        quote! {
            ::otter_vm::AccessorSpec {
                name: #js_name,
                get: #get,
                set: #set,
                attrs: ::otter_vm::Attr::new(false, false, true),
            }
        }
    });

    quote! {
        #module

        #[allow(non_upper_case_globals)]
        static #static_methods_ident: &[::otter_vm::MethodSpec] = &[
            #(#static_method_entries),*
        ];

        #[allow(non_upper_case_globals)]
        static #prototype_methods_ident: &[::otter_vm::MethodSpec] = &[
            #(#prototype_method_entries),*
        ];

        #[allow(non_upper_case_globals)]
        static #prototype_accessors_ident: &[::otter_vm::AccessorSpec] = &[
            #(#prototype_accessor_entries),*
        ];

        #[allow(non_upper_case_globals)]
        #[doc = "Generated static JavaScript class spec."]
        pub static #spec_ident: ::otter_vm::ClassSpec = ::otter_vm::ClassSpec {
            constructor: ::otter_vm::ConstructorSpec {
                name: #class_name,
                length: #constructor_length,
                call: ::otter_vm::NativeCall::Static(#mod_ident::#constructor_rust_name),
                static_methods: #static_methods_ident,
                prototype_methods: #prototype_methods_ident,
                attrs: ::otter_vm::Attr::global_binding(),
            },
            prototype_accessors: #prototype_accessors_ident,
        };
    }
    .into()
}

/// Generate a grouped static namespace spec without helper
/// attributes.
///
/// ```rust,ignore
/// otter_macros::raft! {
///     pub static MATH_SPEC: namespace("Math") {
///         methods: [
///             "abs" => math_abs, length = 1;
///         ]
///     }
/// }
/// ```
///
/// This macro is intentionally declarative: exported JS names and
/// arity remain explicit, and generated methods use
/// `NativeCall::Static`.
#[proc_macro]
pub fn raft(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as RaftInput);
    let spec_ident = input.spec_ident;
    let vis = input.vis;
    let namespace_name = input.namespace_name;
    let methods_ident = format_ident!("__OTTER_{}_METHODS", spec_ident);
    let mut seen = BTreeSet::new();
    for method in &input.methods {
        if !seen.insert(method.js_name.value()) {
            return syn::Error::new_spanned(
                &method.js_name,
                format!("duplicate raft method name `{}`", method.js_name.value()),
            )
            .to_compile_error()
            .into();
        }
    }
    let method_entries = input.methods.iter().map(|method| {
        let js_name = &method.js_name;
        let length = method.length;
        let call = &method.call;
        quote! {
            ::otter_vm::MethodSpec {
                name: #js_name,
                length: #length,
                attrs: ::otter_vm::Attr::builtin_function(),
                call: ::otter_vm::NativeCall::Static(#call),
            }
        }
    });

    quote! {
        #[allow(non_upper_case_globals)]
        static #methods_ident: &[::otter_vm::MethodSpec] = &[
            #(#method_entries),*
        ];

        #[allow(non_upper_case_globals)]
        #[doc = "Generated grouped static JavaScript namespace spec."]
        #vis static #spec_ident: ::otter_vm::NamespaceSpec = ::otter_vm::NamespaceSpec {
            name: #namespace_name,
            methods: #methods_ident,
            accessors: &[],
            constants: &[],
            attrs: ::otter_vm::Attr::global_binding(),
        };
    }
    .into()
}

struct NamespaceArgs {
    name: LitStr,
    spec: Ident,
}

impl Parse for NamespaceArgs {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let mut name = None;
        let mut spec = None;
        while !input.is_empty() {
            let key: Ident = input.parse()?;
            input.parse::<Token![=]>()?;
            match key.to_string().as_str() {
                "name" => name = Some(input.parse()?),
                "spec" => spec = Some(input.parse()?),
                other => {
                    return Err(syn::Error::new(
                        key.span(),
                        format!("unknown js_namespace argument `{other}`"),
                    ));
                }
            }
            if input.peek(Token![,]) {
                input.parse::<Token![,]>()?;
            }
        }
        Ok(Self {
            name: name.ok_or_else(|| input.error("missing `name = \"...\"`"))?,
            spec: spec.ok_or_else(|| input.error("missing `spec = IDENT`"))?,
        })
    }
}

struct ClassArgs {
    name: LitStr,
    spec: Ident,
}

struct RaftInput {
    vis: Visibility,
    spec_ident: Ident,
    namespace_name: LitStr,
    methods: Vec<RaftMethod>,
}

impl Parse for RaftInput {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let vis = input.parse()?;
        input.parse::<Token![static]>()?;
        let spec_ident = input.parse()?;
        input.parse::<Token![:]>()?;
        let kind: Ident = input.parse()?;
        if kind != "namespace" {
            return Err(syn::Error::new(
                kind.span(),
                "raft currently supports namespace specs",
            ));
        }
        let parens;
        parenthesized!(parens in input);
        let namespace_name = parens.parse()?;
        let body;
        braced!(body in input);
        let field: Ident = body.parse()?;
        if field != "methods" {
            return Err(syn::Error::new(field.span(), "expected `methods: [...]`"));
        }
        body.parse::<Token![:]>()?;
        let methods_body;
        bracketed!(methods_body in body);
        let mut methods = Vec::new();
        while !methods_body.is_empty() {
            methods.push(methods_body.parse()?);
            if methods_body.peek(Token![,]) {
                methods_body.parse::<Token![,]>()?;
            }
        }
        Ok(Self {
            vis,
            spec_ident,
            namespace_name,
            methods,
        })
    }
}

struct RaftMethod {
    js_name: LitStr,
    call: Path,
    length: u8,
}

impl Parse for RaftMethod {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let js_name = input.parse()?;
        input.parse::<Token![=>]>()?;
        let call = input.parse()?;
        input.parse::<Token![,]>()?;
        let length_key: Ident = input.parse()?;
        if length_key != "length" {
            return Err(syn::Error::new(length_key.span(), "expected `length = N`"));
        }
        input.parse::<Token![=]>()?;
        let length: LitInt = input.parse()?;
        if input.peek(Token![;]) {
            input.parse::<Token![;]>()?;
        }
        Ok(Self {
            js_name,
            call,
            length: length.base10_parse()?,
        })
    }
}

impl Parse for ClassArgs {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let args = NamespaceArgs::parse(input)?;
        Ok(Self {
            name: args.name,
            spec: args.spec,
        })
    }
}

struct FnArgs {
    name: LitStr,
    length: u8,
}

impl Parse for FnArgs {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let mut name = None;
        let mut length = None;
        while !input.is_empty() {
            let key: Ident = input.parse()?;
            input.parse::<Token![=]>()?;
            match key.to_string().as_str() {
                "name" => name = Some(input.parse()?),
                "length" => {
                    let n: LitInt = input.parse()?;
                    length = Some(n.base10_parse::<u8>()?);
                }
                other => {
                    return Err(syn::Error::new(
                        key.span(),
                        format!("unknown js_fn argument `{other}`"),
                    ));
                }
            }
            if input.peek(Token![,]) {
                input.parse::<Token![,]>()?;
            }
        }
        Ok(Self {
            name: name.ok_or_else(|| input.error("missing `name = \"...\"`"))?,
            length: length.ok_or_else(|| input.error("missing `length = N`"))?,
        })
    }
}

struct LengthArgs {
    length: u8,
}

impl Parse for LengthArgs {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let mut length = None;
        while !input.is_empty() {
            let key: Ident = input.parse()?;
            input.parse::<Token![=]>()?;
            match key.to_string().as_str() {
                "length" => {
                    let n: LitInt = input.parse()?;
                    length = Some(n.base10_parse::<u8>()?);
                }
                other => {
                    return Err(syn::Error::new(
                        key.span(),
                        format!("unknown js_constructor argument `{other}`"),
                    ));
                }
            }
            if input.peek(Token![,]) {
                input.parse::<Token![,]>()?;
            }
        }
        Ok(Self {
            length: length.ok_or_else(|| input.error("missing `length = N`"))?,
        })
    }
}

struct NameArgs {
    name: LitStr,
}

impl Parse for NameArgs {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let mut name = None;
        while !input.is_empty() {
            let key: Ident = input.parse()?;
            input.parse::<Token![=]>()?;
            match key.to_string().as_str() {
                "name" => name = Some(input.parse()?),
                other => {
                    return Err(syn::Error::new(
                        key.span(),
                        format!("unknown accessor argument `{other}`"),
                    ));
                }
            }
            if input.peek(Token![,]) {
                input.parse::<Token![,]>()?;
            }
        }
        Ok(Self {
            name: name.ok_or_else(|| input.error("missing `name = \"...\"`"))?,
        })
    }
}

struct MethodBinding {
    rust_name: Ident,
    js_name: LitStr,
    length: u8,
}

struct ConstructorBinding {
    rust_name: Ident,
    length: u8,
}

struct AccessorBinding {
    js_name: LitStr,
    get: Option<Ident>,
    set: Option<Ident>,
}

impl AccessorBinding {
    fn new(js_name: LitStr) -> Self {
        Self {
            js_name,
            get: None,
            set: None,
        }
    }
}

fn take_js_fn(attrs: &mut Vec<Attribute>) -> Result<Option<FnArgs>> {
    take_fn_attr(attrs, "js_fn")
}

fn take_fn_attr(attrs: &mut Vec<Attribute>, name: &str) -> Result<Option<FnArgs>> {
    let mut found = None;
    let mut retained = Vec::with_capacity(attrs.len());
    for attr in attrs.drain(..) {
        if !attr.path().is_ident(name) {
            retained.push(attr);
            continue;
        }
        if found.is_some() {
            return Err(syn::Error::new_spanned(
                attr,
                format!("duplicate {name} attribute on one function"),
            ));
        }
        found = Some(parse_attr_args(&attr, name)?);
    }
    *attrs = retained;
    Ok(found)
}

fn take_length_attr(attrs: &mut Vec<Attribute>, name: &str) -> Result<Option<LengthArgs>> {
    let mut found = None;
    let mut retained = Vec::with_capacity(attrs.len());
    for attr in attrs.drain(..) {
        if !attr.path().is_ident(name) {
            retained.push(attr);
            continue;
        }
        if found.is_some() {
            return Err(syn::Error::new_spanned(
                attr,
                format!("duplicate {name} attribute on one function"),
            ));
        }
        found = Some(parse_attr_args(&attr, name)?);
    }
    *attrs = retained;
    Ok(found)
}

fn take_name_attr(attrs: &mut Vec<Attribute>, name: &str) -> Result<Option<NameArgs>> {
    let mut found = None;
    let mut retained = Vec::with_capacity(attrs.len());
    for attr in attrs.drain(..) {
        if !attr.path().is_ident(name) {
            retained.push(attr);
            continue;
        }
        if found.is_some() {
            return Err(syn::Error::new_spanned(
                attr,
                format!("duplicate {name} attribute on one function"),
            ));
        }
        found = Some(parse_attr_args(&attr, name)?);
    }
    *attrs = retained;
    Ok(found)
}

fn parse_attr_args<T: Parse>(attr: &Attribute, name: &str) -> Result<T> {
    match &attr.meta {
        Meta::List(list) => list.parse_args(),
        Meta::Path(_) => Err(syn::Error::new_spanned(
            attr,
            format!("{name} requires arguments"),
        )),
        Meta::NameValue(_) => Err(syn::Error::new_spanned(
            attr,
            format!("{name} expects list-style arguments"),
        )),
    }
}
