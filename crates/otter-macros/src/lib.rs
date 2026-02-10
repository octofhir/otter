//! # Otter Macros
//!
//! Proc macros for defining operations and extensions in the Otter VM.
//!
//! ## Otter Terminology
//!
//! | Term | Meaning |
//! |------|---------|
//! | **dive** | A native function (otters dive for fish) |
//! | **deep** | Async dive that returns a Promise |
//!
//! ## `#[dive]` — Native-First Function Binding
//!
//! Generates a `NativeFn` (the VM's native function type) directly, using
//! `FromValue`/`IntoValue` for automatic type marshalling. No serde.
//!
//! ### Parameters
//!
//! - `name = "jsName"` — JavaScript-visible name (default: Rust fn name)
//! - `length = N` — `.length` property (auto-inferred from typed param count)
//! - `method` — marks as instance method (has `this`)
//!
//! ### Supported Signatures
//!
//! **Pattern A — Full native (pass-through):**
//! ```ignore
//! #[dive(name = "push", length = 1)]
//! fn array_push(this: &Value, args: &[Value], ncx: &mut NativeContext) -> Result<Value, VmError> { .. }
//! ```
//!
//! **Pattern B — Args + NativeContext:**
//! ```ignore
//! #[dive(name = "join", length = 0)]
//! fn path_join(args: &[Value], ncx: &mut NativeContext) -> Result<Value, VmError> { .. }
//! ```
//!
//! **Pattern C — Typed params (auto-conversion via FromValue/IntoValue):**
//! ```ignore
//! #[dive(name = "abs", length = 1)]
//! fn math_abs(x: f64) -> f64 { x.abs() }
//! ```
//!
//! ## `dive_module!` — Extension Declaration
//!
//! Generates an `OtterExtension` impl from a list of `#[dive]` functions.
//!
//! ```ignore
//! dive_module!(
//!     node_path,
//!     profiles = [SafeCore, Full],
//!     module_specifiers = ["node:path", "path"],
//!     fns = [path_join, path_dirname, path_basename],
//! );
//! ```

use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::{
    FnArg, Ident, ItemFn, ItemImpl, ItemStruct, LitInt, LitStr, Token, Type,
    parse::{Parse, ParseStream},
    parse_macro_input,
    punctuated::Punctuated,
};

// =============================================================================
// #[dive] macro
// =============================================================================

/// Arguments to the #[dive] attribute
struct DiveArgs {
    /// Custom JS name
    name: Option<String>,
    /// Function .length property
    length: Option<u32>,
    /// Whether this is an async (deep) dive
    deep: bool,
    /// Whether this is an instance method
    method: bool,
}

impl Default for DiveArgs {
    fn default() -> Self {
        Self {
            name: None,
            length: None,
            deep: false,
            method: false,
        }
    }
}

impl Parse for DiveArgs {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        if input.is_empty() {
            return Ok(Self::default());
        }

        let mut args = DiveArgs::default();

        // Parse comma-separated key=value pairs and flags
        loop {
            if input.is_empty() {
                break;
            }

            let ident: Ident = input.parse()?;
            match ident.to_string().as_str() {
                "name" => {
                    input.parse::<Token![=]>()?;
                    let lit: LitStr = input.parse()?;
                    args.name = Some(lit.value());
                }
                "length" => {
                    input.parse::<Token![=]>()?;
                    let lit: LitInt = input.parse()?;
                    args.length = Some(lit.base10_parse()?);
                }
                "deep" => {
                    args.deep = true;
                }
                "method" => {
                    args.method = true;
                }
                other => {
                    return Err(syn::Error::new_spanned(
                        ident,
                        format!(
                            "Unknown dive argument '{}'. Expected 'name', 'length', 'deep', or 'method'.",
                            other
                        ),
                    ));
                }
            }

            if input.peek(Token![,]) {
                input.parse::<Token![,]>()?;
            } else {
                break;
            }
        }

        Ok(args)
    }
}

/// Detected parameter pattern for the function
enum ParamPattern {
    /// `(this: &Value, args: &[Value], ncx: &mut NativeContext) -> Result<Value, VmError>`
    FullNative,
    /// `(args: &[Value], ncx: &mut NativeContext) -> Result<Value, VmError>`
    ArgsAndNcx,
    /// `(ncx: &mut NativeContext) -> Result<T, VmError>` (only NativeContext)
    NcxOnly,
    /// Typed parameters via FromValue/IntoValue
    Typed {
        /// Parameter names and types for extraction
        params: Vec<(Ident, Box<Type>)>,
    },
}

/// Detect which parameter pattern the function uses.
fn detect_pattern(input: &ItemFn) -> ParamPattern {
    let params: Vec<_> = input.sig.inputs.iter().collect();

    // Check for reference types that indicate native patterns
    if params.len() >= 3 {
        if is_value_ref(&params[0]) && is_value_slice(&params[1]) && is_native_context(&params[2])
        {
            return ParamPattern::FullNative;
        }
    }

    if params.len() >= 2 {
        if is_value_slice(&params[0]) && is_native_context(&params[1]) {
            return ParamPattern::ArgsAndNcx;
        }
    }

    if params.len() == 1 && is_native_context(&params[0]) {
        return ParamPattern::NcxOnly;
    }

    // Typed parameters
    let mut typed_params = Vec::new();
    for arg in &params {
        if let FnArg::Typed(pat_type) = arg {
            if let syn::Pat::Ident(pat_ident) = &*pat_type.pat {
                // Skip ncx parameter if it's the last one
                if is_native_context(arg) {
                    continue;
                }
                typed_params.push((pat_ident.ident.clone(), pat_type.ty.clone()));
            }
        }
    }

    ParamPattern::Typed {
        params: typed_params,
    }
}

/// Check if a FnArg is `&Value` (reference to Value)
fn is_value_ref(arg: &FnArg) -> bool {
    if let FnArg::Typed(pat_type) = arg {
        let ty_str = quote!(#pat_type.ty).to_string();
        // Check the actual type
        if let Type::Reference(type_ref) = &*pat_type.ty {
            if let Type::Path(type_path) = &*type_ref.elem {
                if let Some(seg) = type_path.path.segments.last() {
                    return seg.ident == "Value";
                }
            }
        }
        // Fallback: string match
        return ty_str.contains("& Value") || ty_str.contains("&Value");
    }
    false
}

/// Check if a FnArg is `&[Value]` (slice of Value)
fn is_value_slice(arg: &FnArg) -> bool {
    if let FnArg::Typed(pat_type) = arg {
        if let Type::Reference(type_ref) = &*pat_type.ty {
            if let Type::Slice(type_slice) = &*type_ref.elem {
                if let Type::Path(type_path) = &*type_slice.elem {
                    if let Some(seg) = type_path.path.segments.last() {
                        return seg.ident == "Value";
                    }
                }
            }
        }
    }
    false
}

/// Check if a FnArg is `&mut NativeContext` or `&mut NativeContext<'_>`
fn is_native_context(arg: &FnArg) -> bool {
    if let FnArg::Typed(pat_type) = arg {
        if let Type::Reference(type_ref) = &*pat_type.ty {
            if type_ref.mutability.is_some() {
                if let Type::Path(type_path) = &*type_ref.elem {
                    if let Some(seg) = type_path.path.segments.last() {
                        return seg.ident == "NativeContext";
                    }
                }
            }
        }
    }
    false
}

/// Check if the last param is `&mut NativeContext`
fn has_trailing_ncx(input: &ItemFn) -> bool {
    input
        .sig
        .inputs
        .last()
        .map(|arg| is_native_context(arg))
        .unwrap_or(false)
}

/// Marks a function as callable from JavaScript.
///
/// Like an otter diving for fish — goes deep into native code and surfaces with results.
///
/// ## Parameters
///
/// - `name = "jsName"` — JavaScript-visible name (default: Rust fn name)
/// - `length = N` — `.length` property for the JS function (auto-inferred for typed params)
/// - `method` — marks as instance method
/// - `deep` — async operation (returns Promise)
///
/// ## Generated Code
///
/// The macro generates:
/// 1. The original function (unchanged)
/// 2. `fn_name_native_fn() -> NativeFn` — creates the NativeFn wrapper (cached via OnceLock)
/// 3. `FN_NAME_NAME: &str` — the JS name constant
/// 4. `FN_NAME_LENGTH: u32` — the length constant
/// 5. `fn_name_decl() -> (&'static str, NativeFn, u32)` — convenience tuple
///
/// ## Example
///
/// ```ignore
/// use otter_macros::dive;
///
/// // length auto-inferred as 1 from typed param count
/// #[dive(name = "abs")]
/// fn math_abs(x: f64) -> f64 { x.abs() }
///
/// // length explicit for varargs patterns
/// #[dive(name = "join", length = 0)]
/// fn path_join(args: &[Value], ncx: &mut NativeContext) -> Result<Value, VmError> { .. }
/// ```
#[proc_macro_attribute]
pub fn dive(attr: TokenStream, item: TokenStream) -> TokenStream {
    let args = parse_macro_input!(attr as DiveArgs);
    let input = parse_macro_input!(item as ItemFn);

    expand_dive_native(input, args)
}

/// Expand #[dive] to generate NativeFn-based code
fn expand_dive_native(input: ItemFn, args: DiveArgs) -> TokenStream {
    let fn_name = &input.sig.ident;
    let vis = &input.vis;
    let pattern = detect_pattern(&input);

    // Auto-infer length from typed params count if not explicitly set
    let length = args.length.unwrap_or_else(|| match &pattern {
        ParamPattern::Typed { params } => params.len() as u32,
        _ => 0,
    });

    // JS name (from attr or function name)
    let js_name = args.name.unwrap_or_else(|| fn_name.to_string());

    // Generated identifiers
    let native_fn_ident = format_ident!("{}_native_fn", fn_name);
    let decl_ident = format_ident!("{}_decl", fn_name);
    let name_const = format_ident!("{}_NAME", fn_name.to_string().to_uppercase());
    let length_const = format_ident!("{}_LENGTH", fn_name.to_string().to_uppercase());

    let wrapper_body = match pattern {
        ParamPattern::FullNative => {
            // Function already has the NativeFn signature — just call it directly
            quote! {
                #fn_name(_this, _args, _ncx)
            }
        }
        ParamPattern::ArgsAndNcx => {
            // Function takes (args, ncx) — pass through
            quote! {
                #fn_name(_args, _ncx)
            }
        }
        ParamPattern::NcxOnly => {
            // Function takes only ncx
            let return_handling = generate_return_handling(&input);
            match return_handling {
                ReturnHandling::ResultValue => quote! {
                    #fn_name(_ncx)
                },
                ReturnHandling::ResultTyped => quote! {
                    match #fn_name(_ncx) {
                        Ok(v) => Ok(otter_vm_core::convert::IntoValue::into_value(v)),
                        Err(e) => Err(e),
                    }
                },
                ReturnHandling::PlainTyped => quote! {
                    Ok(otter_vm_core::convert::IntoValue::into_value(#fn_name(_ncx)))
                },
                ReturnHandling::Unit => quote! {
                    #fn_name(_ncx);
                    Ok(otter_vm_core::value::Value::undefined())
                },
            }
        }
        ParamPattern::Typed { ref params } => {
            // Extract typed parameters via FromValue
            let has_ncx = has_trailing_ncx(&input);
            let extractions: Vec<_> = params
                .iter()
                .enumerate()
                .map(|(i, (name, ty))| {
                    quote! {
                        let #name: #ty = otter_vm_core::convert::FromValue::from_value(
                            _args.get(#i).unwrap_or(&otter_vm_core::value::Value::undefined())
                        )?;
                    }
                })
                .collect();

            let arg_names: Vec<_> = params.iter().map(|(name, _)| name.clone()).collect();
            let call = if has_ncx {
                quote! { #fn_name(#(#arg_names),*, _ncx) }
            } else {
                quote! { #fn_name(#(#arg_names),*) }
            };

            let return_handling = generate_return_handling(&input);
            let invocation = match return_handling {
                ReturnHandling::ResultValue => quote! { #call },
                ReturnHandling::ResultTyped => quote! {
                    match #call {
                        Ok(v) => Ok(otter_vm_core::convert::IntoValue::into_value(v)),
                        Err(e) => Err(e),
                    }
                },
                ReturnHandling::PlainTyped => quote! {
                    Ok(otter_vm_core::convert::IntoValue::into_value(#call))
                },
                ReturnHandling::Unit => quote! {
                    #call;
                    Ok(otter_vm_core::value::Value::undefined())
                },
            };

            quote! {
                #(#extractions)*
                #invocation
            }
        }
    };

    let expanded = quote! {
        // Original function (unchanged)
        #input

        /// JS name for this dive function.
        #vis const #name_const: &str = #js_name;

        /// JS `.length` for this dive function.
        #vis const #length_const: u32 = #length;

        /// Get the cached `NativeFn` wrapper for this dive function.
        #vis fn #native_fn_ident() -> std::sync::Arc<
            dyn Fn(
                &otter_vm_core::value::Value,
                &[otter_vm_core::value::Value],
                &mut otter_vm_core::context::NativeContext<'_>,
            ) -> std::result::Result<otter_vm_core::value::Value, otter_vm_core::error::VmError>
                + Send
                + Sync,
        > {
            type NativeFnArc = std::sync::Arc<
                dyn Fn(
                    &otter_vm_core::value::Value,
                    &[otter_vm_core::value::Value],
                    &mut otter_vm_core::context::NativeContext<'_>,
                ) -> std::result::Result<otter_vm_core::value::Value, otter_vm_core::error::VmError>
                    + Send
                    + Sync,
            >;
            static CACHED: std::sync::OnceLock<NativeFnArc> = std::sync::OnceLock::new();
            CACHED.get_or_init(|| {
                std::sync::Arc::new(|_this: &otter_vm_core::value::Value,
                                      _args: &[otter_vm_core::value::Value],
                                      _ncx: &mut otter_vm_core::context::NativeContext<'_>|
                    -> std::result::Result<otter_vm_core::value::Value, otter_vm_core::error::VmError> {
                    #wrapper_body
                })
            }).clone()
        }

        /// Convenience: `(name, native_fn, length)` tuple for module registration.
        #vis fn #decl_ident() -> (
            &'static str,
            std::sync::Arc<
                dyn Fn(
                    &otter_vm_core::value::Value,
                    &[otter_vm_core::value::Value],
                    &mut otter_vm_core::context::NativeContext<'_>,
                ) -> std::result::Result<otter_vm_core::value::Value, otter_vm_core::error::VmError>
                    + Send
                    + Sync,
            >,
            u32,
        ) {
            (#name_const, #native_fn_ident(), #length_const)
        }
    };

    TokenStream::from(expanded)
}

/// How to handle the return value
enum ReturnHandling {
    /// Already returns `Result<Value, VmError>` — pass through
    ResultValue,
    /// Returns `Result<T, VmError>` where T: IntoValue
    ResultTyped,
    /// Returns a plain type T: IntoValue
    PlainTyped,
    /// Returns () — map to undefined
    Unit,
}

fn generate_return_handling(input: &ItemFn) -> ReturnHandling {
    match &input.sig.output {
        syn::ReturnType::Default => ReturnHandling::Unit,
        syn::ReturnType::Type(_, ty) => {
            // Check for Result<T, E>
            if let Type::Path(type_path) = ty.as_ref() {
                if let Some(seg) = type_path.path.segments.last() {
                    if seg.ident == "Result" {
                        // Check if the Ok type is Value
                        if let syn::PathArguments::AngleBracketed(args) = &seg.arguments {
                            if let Some(syn::GenericArgument::Type(ok_type)) = args.args.first() {
                                if is_value_type(ok_type) {
                                    return ReturnHandling::ResultValue;
                                }
                            }
                        }
                        return ReturnHandling::ResultTyped;
                    }
                }
            }
            // Check for unit type ()
            if let Type::Tuple(tuple) = ty.as_ref() {
                if tuple.elems.is_empty() {
                    return ReturnHandling::Unit;
                }
            }
            ReturnHandling::PlainTyped
        }
    }
}

/// Check if a type is `Value`
fn is_value_type(ty: &Type) -> bool {
    if let Type::Path(type_path) = ty {
        if let Some(seg) = type_path.path.segments.last() {
            return seg.ident == "Value";
        }
    }
    false
}

// =============================================================================
// dive_module! macro
// =============================================================================

/// Input for `dive_module!` macro
struct DiveModuleInput {
    /// Extension struct name (e.g., `node_path` -> `NodePathExtension`)
    name: Ident,
    /// Dependencies
    deps: Vec<LitStr>,
    /// Profiles
    profiles: Vec<Ident>,
    /// Module specifiers (e.g., "node:path", "path")
    module_specifiers: Vec<LitStr>,
    /// Function names to include in the module
    fns: Vec<Ident>,
    /// Static properties (name => expr pairs)
    properties: Vec<(LitStr, syn::Expr)>,
}

impl Parse for DiveModuleInput {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        // Parse extension name
        let name: Ident = input.parse()?;
        input.parse::<Token![,]>()?;

        let mut deps = Vec::new();
        let mut profiles = Vec::new();
        let mut module_specifiers = Vec::new();
        let mut fns = Vec::new();
        let mut properties = Vec::new();

        // Parse remaining key = value pairs
        while !input.is_empty() {
            let key: Ident = input.parse()?;
            input.parse::<Token![=]>()?;

            match key.to_string().as_str() {
                "deps" => {
                    let content;
                    syn::bracketed!(content in input);
                    deps = Punctuated::<LitStr, Token![,]>::parse_terminated(&content)?
                        .into_iter()
                        .collect();
                }
                "profiles" => {
                    let content;
                    syn::bracketed!(content in input);
                    profiles = Punctuated::<Ident, Token![,]>::parse_terminated(&content)?
                        .into_iter()
                        .collect();
                }
                "module_specifiers" => {
                    let content;
                    syn::bracketed!(content in input);
                    module_specifiers =
                        Punctuated::<LitStr, Token![,]>::parse_terminated(&content)?
                            .into_iter()
                            .collect();
                }
                "fns" => {
                    let content;
                    syn::bracketed!(content in input);
                    fns = Punctuated::<Ident, Token![,]>::parse_terminated(&content)?
                        .into_iter()
                        .collect();
                }
                "properties" => {
                    let content;
                    syn::braced!(content in input);
                    while !content.is_empty() {
                        let prop_name: LitStr = content.parse()?;
                        content.parse::<Token![=>]>()?;
                        let prop_expr: syn::Expr = content.parse()?;
                        properties.push((prop_name, prop_expr));
                        if content.peek(Token![,]) {
                            content.parse::<Token![,]>()?;
                        }
                    }
                }
                other => {
                    return Err(syn::Error::new_spanned(
                        &key,
                        format!(
                            "Unknown dive_module option '{}'. Expected: deps, profiles, module_specifiers, fns, properties.",
                            other
                        ),
                    ));
                }
            }

            if input.peek(Token![,]) {
                input.parse::<Token![,]>()?;
            }
        }

        Ok(Self {
            name,
            deps,
            profiles,
            module_specifiers,
            fns,
            properties,
        })
    }
}

/// Declare a native extension module.
///
/// Generates an `OtterExtension` implementation from `#[dive]` functions.
///
/// ## Example
///
/// ```ignore
/// dive_module!(
///     node_path,
///     profiles = [SafeCore, Full],
///     module_specifiers = ["node:path", "path"],
///     fns = [path_join, path_dirname, path_basename],
///     properties = {
///         "sep" => Value::string(JsString::intern("/")),
///     },
/// );
/// ```
///
/// This generates:
/// - `pub struct NodePathExtension;`
/// - `impl OtterExtension for NodePathExtension { ... }`
/// - `pub fn node_path_extension() -> Box<dyn OtterExtension>`
#[proc_macro]
pub fn dive_module(input: TokenStream) -> TokenStream {
    let module_input = parse_macro_input!(input as DiveModuleInput);
    expand_dive_module(module_input)
}

fn expand_dive_module(input: DiveModuleInput) -> TokenStream {
    let ext_name_str = input.name.to_string();

    // Convert snake_case name to PascalCase for the struct
    let struct_name_str = to_pascal_case(&ext_name_str);
    let struct_name = format_ident!("{}Extension", struct_name_str);

    // Factory function name
    let factory_fn = format_ident!("{}_extension", ext_name_str);

    // Deps
    let deps: Vec<_> = input.deps.iter().collect();
    let deps_len = deps.len();

    // Profiles
    let profile_variants: Vec<_> = input
        .profiles
        .iter()
        .map(|p| {
            let variant = format_ident!("{}", p);
            quote! { otter_vm_runtime::extension_v2::Profile::#variant }
        })
        .collect();
    let profiles_len = profile_variants.len();

    // Module specifiers
    let specifiers: Vec<_> = input.module_specifiers.iter().collect();
    let specifiers_len = specifiers.len();

    // Function registrations for load_module
    let fn_registrations: Vec<_> = input
        .fns
        .iter()
        .map(|fn_name| {
            let decl_fn = format_ident!("{}_decl", fn_name);
            quote! {
                {
                    let (name, native_fn, length) = #decl_fn();
                    ns = ns.function(name, native_fn, length);
                }
            }
        })
        .collect();

    // Property registrations
    let prop_registrations: Vec<_> = input
        .properties
        .iter()
        .map(|(name, expr)| {
            quote! {
                ns = ns.property(#name, #expr);
            }
        })
        .collect();

    let expanded = quote! {
        /// Auto-generated extension struct for the `#ext_name_str` module.
        pub struct #struct_name;

        impl otter_vm_runtime::extension_v2::OtterExtension for #struct_name {
            fn name(&self) -> &str {
                #ext_name_str
            }

            fn profiles(&self) -> &[otter_vm_runtime::extension_v2::Profile] {
                static PROFILES: [otter_vm_runtime::extension_v2::Profile; #profiles_len] =
                    [#(#profile_variants),*];
                &PROFILES
            }

            fn deps(&self) -> &[&str] {
                static DEPS: [&str; #deps_len] = [#(#deps),*];
                &DEPS
            }

            fn module_specifiers(&self) -> &[&str] {
                static SPECIFIERS: [&str; #specifiers_len] = [#(#specifiers),*];
                &SPECIFIERS
            }

            fn install(
                &self,
                _ctx: &mut otter_vm_runtime::registration::RegistrationContext,
            ) -> std::result::Result<(), otter_vm_core::error::VmError> {
                Ok(())
            }

            fn load_module(
                &self,
                _specifier: &str,
                ctx: &mut otter_vm_runtime::registration::RegistrationContext,
            ) -> Option<otter_vm_core::gc::GcRef<otter_vm_core::object::JsObject>> {
                let mut ns = ctx.module_namespace();
                #(#fn_registrations)*
                #(#prop_registrations)*
                Some(ns.build())
            }
        }

        /// Create a boxed extension instance for registration.
        pub fn #factory_fn() -> Box<dyn otter_vm_runtime::extension_v2::OtterExtension> {
            Box::new(#struct_name)
        }
    };

    TokenStream::from(expanded)
}

/// Convert snake_case to PascalCase
fn to_pascal_case(s: &str) -> String {
    s.split('_')
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                None => String::new(),
                Some(c) => c.to_uppercase().chain(chars).collect(),
            }
        })
        .collect()
}

// =============================================================================
// #[js_class] macro
// =============================================================================

/// Arguments for js_class
#[derive(Default)]
struct JsClassArgs {
    name: Option<String>,
}

impl Parse for JsClassArgs {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let mut name = None;

        while !input.is_empty() {
            let ident: Ident = input.parse()?;
            input.parse::<Token![=]>()?;

            if ident == "name" {
                let lit: LitStr = input.parse()?;
                name = Some(lit.value());
            } else {
                return Err(syn::Error::new_spanned(
                    ident,
                    "Unknown js_class option. Expected 'name'.",
                ));
            }

            if input.peek(Token![,]) {
                input.parse::<Token![,]>()?;
            }
        }

        Ok(Self { name })
    }
}

/// Mark a struct or impl block as a JavaScript class.
///
/// Generates the necessary boilerplate for exposing a Rust struct as a JavaScript
/// class in the Otter VM.
///
/// ## Options
///
/// - `name = "ClassName"` — Custom JavaScript class name (default: struct name)
///
/// ## Field Attributes
///
/// - `#[js_readonly]` — Expose field as read-only property
/// - `#[js_skip]` — Don't expose this field to JavaScript
///
/// ## Method Attributes (on impl block)
///
/// - `#[js_constructor]` — Mark as constructor
/// - `#[js_method]` — Mark as instance method
/// - `#[js_static]` — Mark as static method
/// - `#[js_getter]` — Mark as property getter
/// - `#[js_setter]` — Mark as property setter
#[proc_macro_attribute]
pub fn js_class(attr: TokenStream, item: TokenStream) -> TokenStream {
    let args = parse_macro_input!(attr as JsClassArgs);

    let item_clone = item.clone();
    if let Ok(s) = syn::parse::<ItemStruct>(item_clone) {
        return expand_js_class_struct(s, args);
    }

    let item_clone = item.clone();
    if let Ok(i) = syn::parse::<ItemImpl>(item_clone) {
        return expand_js_class_impl(i);
    }

    syn::Error::new(
        proc_macro2::Span::call_site(),
        "Expected struct or impl block",
    )
    .to_compile_error()
    .into()
}

fn expand_js_class_struct(input: ItemStruct, args: JsClassArgs) -> TokenStream {
    let struct_name = &input.ident;
    let vis = &input.vis;
    let attrs = &input.attrs;
    let generics = &input.generics;
    let class_name = args.name.unwrap_or_else(|| struct_name.to_string());

    let mut js_properties = Vec::new();
    let mut js_readonly = Vec::new();
    let mut cleaned_fields = Vec::new();

    for field in input.fields.iter() {
        let is_skip = field.attrs.iter().any(|a| a.path().is_ident("js_skip"));
        let is_readonly = field.attrs.iter().any(|a| a.path().is_ident("js_readonly"));

        let cleaned_attrs: Vec<_> = field
            .attrs
            .iter()
            .filter(|a| !a.path().is_ident("js_skip") && !a.path().is_ident("js_readonly"))
            .collect();

        if let Some(ident) = &field.ident {
            if !is_skip {
                if is_readonly {
                    js_readonly.push(ident.clone());
                } else {
                    js_properties.push(ident.clone());
                }
            }
        }

        let field_vis = &field.vis;
        let field_ident = &field.ident;
        let field_ty = &field.ty;
        cleaned_fields.push(quote! {
            #(#cleaned_attrs)*
            #field_vis #field_ident: #field_ty
        });
    }

    let property_names: Vec<_> = js_properties.iter().map(|n| n.to_string()).collect();
    let readonly_names: Vec<_> = js_readonly.iter().map(|n| n.to_string()).collect();

    let expanded = quote! {
        #(#attrs)*
        #vis struct #struct_name #generics {
            #(#cleaned_fields),*
        }

        impl #struct_name {
            /// JavaScript class name
            pub const JS_CLASS_NAME: &'static str = #class_name;

            /// Get writable property names
            pub fn js_properties() -> &'static [&'static str] {
                &[#(#property_names),*]
            }

            /// Get readonly property names
            pub fn js_readonly_properties() -> &'static [&'static str] {
                &[#(#readonly_names),*]
            }
        }
    };

    TokenStream::from(expanded)
}

fn expand_js_class_impl(input: ItemImpl) -> TokenStream {
    let self_ty = &input.self_ty;

    let mut constructors = Vec::new();
    let mut methods = Vec::new();
    let mut static_methods = Vec::new();
    let mut js_getters = Vec::new();
    let mut js_setters = Vec::new();

    for item in &input.items {
        if let syn::ImplItem::Fn(method) = item {
            let is_constructor = method
                .attrs
                .iter()
                .any(|a| a.path().is_ident("js_constructor"));
            let is_static = method.attrs.iter().any(|a| a.path().is_ident("js_static"));
            let is_getter = method.attrs.iter().any(|a| a.path().is_ident("js_getter"));
            let is_setter = method.attrs.iter().any(|a| a.path().is_ident("js_setter"));
            let is_method = method.attrs.iter().any(|a| a.path().is_ident("js_method"));

            let name = method.sig.ident.to_string();

            if is_constructor {
                constructors.push(name);
            } else if is_static {
                static_methods.push(name);
            } else if is_getter {
                js_getters.push(name);
            } else if is_setter {
                js_setters.push(name);
            } else if is_method {
                methods.push(name);
            }
        }
    }

    let expanded = quote! {
        #input

        impl #self_ty {
            /// Get JS constructor names
            pub fn js_constructors() -> &'static [&'static str] {
                &[#(#constructors),*]
            }

            /// Get JS method names
            pub fn js_methods() -> &'static [&'static str] {
                &[#(#methods),*]
            }

            /// Get JS static method names
            pub fn js_static_methods() -> &'static [&'static str] {
                &[#(#static_methods),*]
            }

            /// Get JS getter names
            pub fn js_getters() -> &'static [&'static str] {
                &[#(#js_getters),*]
            }

            /// Get JS setter names
            pub fn js_setters() -> &'static [&'static str] {
                &[#(#js_setters),*]
            }
        }
    };

    TokenStream::from(expanded)
}

// =============================================================================
// Helper attribute macros for #[js_class]
// =============================================================================

/// Mark a method as JS constructor
#[proc_macro_attribute]
pub fn js_constructor(_attr: TokenStream, item: TokenStream) -> TokenStream {
    item
}

/// Mark a method as JS instance method
#[proc_macro_attribute]
pub fn js_method(_attr: TokenStream, item: TokenStream) -> TokenStream {
    item
}

/// Mark a method as JS static method
#[proc_macro_attribute]
pub fn js_static(_attr: TokenStream, item: TokenStream) -> TokenStream {
    item
}

/// Mark a method as JS getter
#[proc_macro_attribute]
pub fn js_getter(_attr: TokenStream, item: TokenStream) -> TokenStream {
    item
}

/// Mark a method as JS setter
#[proc_macro_attribute]
pub fn js_setter(_attr: TokenStream, item: TokenStream) -> TokenStream {
    item
}

#[cfg(test)]
mod tests {
    // Proc-macro tests require integration tests or trybuild
}
