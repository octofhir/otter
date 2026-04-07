//! # Otter Macros
//!
//! Proc macros for descriptor-driven JS bindings in the Otter VM.
//!
//! `#[js_class]` / `#[js_namespace]` are the main descriptor macros for the
//! active VM. `#[dive]` is the otter-themed macro for single native bindings
//! and emits descriptor metadata for the active runtime stack.
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
//! It emits `NativeFunctionDescriptor` metadata directly for the active VM.
//!
//! ### Parameters
//!
//! - `name = "jsName"` — JavaScript-visible name (default: Rust fn name)
//! - `length = N` — `.length` property
//! - `method` — marks as instance method (has `this`)
//!
//! ### Required Signature
//!
//! ```ignore
//! use otter_runtime::RuntimeState;
//! use otter_vm::{RegisterValue, VmNativeCallError};
//! use otter_macros::dive;
//!
//! #[dive(name = "now")]
//! fn performance_now(
//!     this: &RegisterValue,
//!     args: &[RegisterValue],
//!     runtime: &mut RuntimeState,
//! ) -> Result<RegisterValue, VmNativeCallError> {
//!     // ...
//! }
//! ```
//!
//! ## `lodge!` — Hosted Module Declaration
//!
//! Generates a `HostedNativeModuleLoader` for the active runtime stack.
//!
//! ```ignore
//! lodge!(
//!     path_module,
//!     module_specifiers = ["node:path", "path"],
//!     default = object,
//!     functions = [
//!         ("join", path_join),
//!         ("dirname", path_dirname),
//!         ("basename", path_basename),
//!     ],
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

mod js_namespace;

// =============================================================================
// #[dive] macro
// =============================================================================

/// Arguments to the #[dive] attribute
#[derive(Default)]
struct DiveArgs {
    /// Custom JS name
    name: Option<String>,
    /// Function .length property
    length: Option<u32>,
    /// Whether this is an async (deep) dive
    deep: bool,
    /// Whether this is an instance method
    method: bool,
    /// Whether this is a getter
    getter: bool,
    /// Whether this is a setter
    setter: bool,
    /// Whether this is a constructor
    constructor: bool,
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
                "getter" => {
                    args.getter = true;
                }
                "setter" => {
                    args.setter = true;
                }
                "constructor" => {
                    args.constructor = true;
                }
                other => {
                    return Err(syn::Error::new_spanned(
                        ident,
                        format!(
                            "Unknown dive argument '{}'. Expected 'name', 'length', 'deep', 'method', 'getter', 'setter', or 'constructor'.",
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum DiveSlotKind {
    Method,
    Getter,
    Setter,
    Constructor,
}

impl DiveArgs {
    fn slot_kind(&self) -> Result<DiveSlotKind, syn::Error> {
        let enabled = [self.method, self.getter, self.setter, self.constructor]
            .into_iter()
            .filter(|flag| *flag)
            .count();

        if enabled > 1 {
            return Err(syn::Error::new(
                proc_macro2::Span::call_site(),
                "dive accepts at most one of: method, getter, setter, constructor",
            ));
        }

        if self.getter {
            Ok(DiveSlotKind::Getter)
        } else if self.setter {
            Ok(DiveSlotKind::Setter)
        } else if self.constructor {
            Ok(DiveSlotKind::Constructor)
        } else {
            Ok(DiveSlotKind::Method)
        }
    }
}

fn is_active_dive_signature(input: &ItemFn) -> bool {
    let params: Vec<_> = input.sig.inputs.iter().collect();
    params.len() == 3
        && is_register_value_ref(params[0])
        && is_register_value_slice(params[1])
        && is_runtime_state_ref(params[2])
}

/// Marks a function as callable from JavaScript.
///
/// Like an otter diving for fish — goes deep into native code and surfaces with results.
///
/// ## Parameters
///
/// - `name = "jsName"` — JavaScript-visible name (default: Rust fn name)
/// - `length = N` — `.length` property for the JS function
/// - `method` — marks as instance method
/// - `deep` — async operation (returns Promise)
///
/// ## Generated Code
///
/// For active runtime signatures, the macro generates:
/// 1. The original function (unchanged)
/// 2. `FN_NAME_NAME: &str` — the JS name constant
/// 3. `FN_NAME_LENGTH: u32` — the length constant
/// 4. `fn_name_descriptor() -> NativeFunctionDescriptor`
/// 5. `fn_name_binding(target) -> NativeBindingDescriptor`
///
/// ## Example
///
/// ```ignore
/// use otter_runtime::RuntimeState;
/// use otter_vm::{RegisterValue, VmNativeCallError};
/// use otter_macros::dive;
///
/// #[dive(name = "now")]
/// fn performance_now(
///     this: &RegisterValue,
///     args: &[RegisterValue],
///     runtime: &mut RuntimeState,
/// ) -> Result<RegisterValue, VmNativeCallError> {
///     // ...
/// }
/// ```
#[proc_macro_attribute]
pub fn dive(attr: TokenStream, item: TokenStream) -> TokenStream {
    let args = parse_macro_input!(attr as DiveArgs);
    let input = parse_macro_input!(item as ItemFn);

    if !is_active_dive_signature(&input) {
        let error = syn::Error::new_spanned(
            &input.sig,
            "#[dive] requires the active runtime signature: fn(&RegisterValue, &[RegisterValue], &mut RuntimeState) -> Result<RegisterValue, VmNativeCallError>",
        )
        .to_compile_error();
        return quote! {
            #input
            #error
        }
        .into();
    }

    expand_dive_active(input, args)
}

fn expand_dive_active(input: ItemFn, args: DiveArgs) -> TokenStream {
    let fn_name = &input.sig.ident;
    let vis = &input.vis;

    let slot_kind = match args.slot_kind() {
        Ok(slot_kind) => slot_kind,
        Err(error) => {
            let error = error.to_compile_error();
            return quote! {
                #input
                #error
            }
            .into();
        }
    };

    if args.deep && slot_kind != DiveSlotKind::Method {
        let error = syn::Error::new_spanned(
            &input.sig.ident,
            "dive(deep) is only supported for method-style active-VM bindings",
        )
        .to_compile_error();
        return quote! {
            #input
            #error
        }
        .into();
    }

    let length = args.length.unwrap_or(match slot_kind {
        DiveSlotKind::Setter => 1,
        _ => 0,
    });
    let js_name = args.name.unwrap_or_else(|| fn_name.to_string());

    let descriptor_ident = format_ident!("{}_descriptor", fn_name);
    let binding_ident = format_ident!("{}_binding", fn_name);
    let name_const = format_ident!("{}_NAME", fn_name.to_string().to_uppercase());
    let length_const = format_ident!("{}_LENGTH", fn_name.to_string().to_uppercase());

    let descriptor_ctor = match slot_kind {
        DiveSlotKind::Method if args.deep => {
            quote! {
                ::otter_vm::NativeFunctionDescriptor::async_method(
                    #name_const,
                    #length_const as u16,
                    callback,
                )
            }
        }
        DiveSlotKind::Method => {
            quote! {
                ::otter_vm::NativeFunctionDescriptor::method(
                    #name_const,
                    #length_const as u16,
                    callback,
                )
            }
        }
        DiveSlotKind::Getter => {
            quote! {
                ::otter_vm::NativeFunctionDescriptor::getter(#name_const, callback)
            }
        }
        DiveSlotKind::Setter => {
            quote! {
                ::otter_vm::NativeFunctionDescriptor::setter(#name_const, callback)
            }
        }
        DiveSlotKind::Constructor => {
            quote! {
                ::otter_vm::NativeFunctionDescriptor::constructor(
                    #name_const,
                    #length_const as u16,
                    callback,
                )
            }
        }
    };

    quote! {
        #input

        /// JS name for this dive function.
        #vis const #name_const: &str = #js_name;

        /// JS `.length` for this dive function.
        #vis const #length_const: u32 = #length;

        /// Active-VM native function descriptor for this dive function.
        #vis fn #descriptor_ident() -> ::otter_vm::NativeFunctionDescriptor {
            let callback = #fn_name as ::otter_vm::VmNativeFunction;
            #descriptor_ctor
        }

        /// Convenience wrapper for installing this dive function on a target.
        #vis fn #binding_ident(
            target: ::otter_vm::NativeBindingTarget,
        ) -> ::otter_vm::NativeBindingDescriptor {
            ::otter_vm::NativeBindingDescriptor::new(target, #descriptor_ident())
        }
    }
    .into()
}

// =============================================================================
// lodge! macro
// =============================================================================

#[derive(Clone)]
struct LodgeFunctionExport {
    export_name: LitStr,
    source: Ident,
    js_name: Option<LitStr>,
}

impl Parse for LodgeFunctionExport {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let content;
        syn::parenthesized!(content in input);
        let export_name: LitStr = content.parse()?;
        content.parse::<Token![,]>()?;
        let source: Ident = content.parse()?;
        let js_name = if content.peek(Token![as]) {
            content.parse::<Token![as]>()?;
            Some(content.parse()?)
        } else {
            None
        };

        Ok(Self {
            export_name,
            source,
            js_name,
        })
    }
}

#[derive(Clone)]
struct LodgeValueExport {
    export_name: LitStr,
    expr: syn::Expr,
}

impl Parse for LodgeValueExport {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let content;
        syn::parenthesized!(content in input);
        let export_name: LitStr = content.parse()?;
        content.parse::<Token![,]>()?;
        let expr: syn::Expr = content.parse()?;
        Ok(Self { export_name, expr })
    }
}

#[derive(Clone)]
struct LodgeDefaultFunction {
    source: Ident,
    js_name: Option<LitStr>,
}

#[derive(Clone)]
enum LodgeDefault {
    Object,
    Function(LodgeDefaultFunction),
    Value(syn::Expr),
}

/// Input for `lodge!` macro
struct LodgeInput {
    name: Ident,
    module_specifiers: Vec<LitStr>,
    functions: Vec<LodgeFunctionExport>,
    values: Vec<LodgeValueExport>,
    default: Option<LodgeDefault>,
}

impl Parse for LodgeInput {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let name: Ident = input.parse()?;
        input.parse::<Token![,]>()?;

        let mut module_specifiers = Vec::new();
        let mut functions = Vec::new();
        let mut values = Vec::new();
        let mut default = None;

        while !input.is_empty() {
            let key: Ident = input.parse()?;
            input.parse::<Token![=]>()?;

            match key.to_string().as_str() {
                "module_specifiers" => {
                    let content;
                    syn::bracketed!(content in input);
                    module_specifiers =
                        Punctuated::<LitStr, Token![,]>::parse_terminated(&content)?
                            .into_iter()
                            .collect();
                }
                "functions" => {
                    let content;
                    syn::bracketed!(content in input);
                    functions =
                        Punctuated::<LodgeFunctionExport, Token![,]>::parse_terminated(&content)?
                            .into_iter()
                            .collect();
                }
                "values" => {
                    let content;
                    syn::bracketed!(content in input);
                    values = Punctuated::<LodgeValueExport, Token![,]>::parse_terminated(&content)?
                        .into_iter()
                        .collect();
                }
                "default" => {
                    let mode: Ident = input.parse()?;
                    default = Some(match mode.to_string().as_str() {
                        "object" => LodgeDefault::Object,
                        "function" => {
                            let content;
                            syn::parenthesized!(content in input);
                            let source: Ident = content.parse()?;
                            let js_name = if content.peek(Token![as]) {
                                content.parse::<Token![as]>()?;
                                Some(content.parse()?)
                            } else {
                                None
                            };
                            LodgeDefault::Function(LodgeDefaultFunction { source, js_name })
                        }
                        "value" => {
                            let content;
                            syn::parenthesized!(content in input);
                            LodgeDefault::Value(content.parse()?)
                        }
                        other => {
                            return Err(syn::Error::new_spanned(
                                mode,
                                format!(
                                    "Unknown lodge! default mode '{other}'. Expected: object, function(...), value(...)."
                                ),
                            ));
                        }
                    });
                }
                other => {
                    return Err(syn::Error::new_spanned(
                        &key,
                        format!(
                            "Unknown lodge! option '{}'. Expected: module_specifiers, functions, values, default.",
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
            module_specifiers,
            functions,
            values,
            default,
        })
    }
}

struct RaftInput {
    target: Ident,
    fns: Vec<Ident>,
}

impl Parse for RaftInput {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let mut target = None;
        let mut fns = Vec::new();

        while !input.is_empty() {
            let key: Ident = input.parse()?;
            input.parse::<Token![=]>()?;

            match key.to_string().as_str() {
                "target" => {
                    target = Some(input.parse()?);
                }
                "fns" => {
                    let content;
                    syn::bracketed!(content in input);
                    fns = Punctuated::<Ident, Token![,]>::parse_terminated(&content)?
                        .into_iter()
                        .collect();
                }
                other => {
                    return Err(syn::Error::new_spanned(
                        key,
                        format!("Unknown raft! option '{other}'. Expected: target, fns."),
                    ));
                }
            }

            if input.peek(Token![,]) {
                input.parse::<Token![,]>()?;
            }
        }

        let target = target.ok_or_else(|| {
            syn::Error::new(
                proc_macro2::Span::call_site(),
                "raft! requires target = ...",
            )
        })?;

        Ok(Self { target, fns })
    }
}

struct BurrowInput {
    fns: Vec<Ident>,
}

impl Parse for BurrowInput {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let mut fns = Vec::new();

        while !input.is_empty() {
            let key: Ident = input.parse()?;
            input.parse::<Token![=]>()?;

            match key.to_string().as_str() {
                "fns" => {
                    let content;
                    syn::bracketed!(content in input);
                    fns = Punctuated::<Ident, Token![,]>::parse_terminated(&content)?
                        .into_iter()
                        .collect();
                }
                other => {
                    return Err(syn::Error::new_spanned(
                        key,
                        format!("Unknown burrow! option '{other}'. Expected: fns."),
                    ));
                }
            }

            if input.peek(Token![,]) {
                input.parse::<Token![,]>()?;
            }
        }

        Ok(Self { fns })
    }
}

/// Declare one hosted module loader for the active runtime stack.
///
/// Generates a `HostedNativeModuleLoader` plus an `*_entries()` helper for
/// registering all declared specifiers on an extension.
#[proc_macro]
pub fn lodge(input: TokenStream) -> TokenStream {
    let module_input = parse_macro_input!(input as LodgeInput);
    expand_lodge_module(module_input)
}

#[proc_macro]
pub fn raft(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as RaftInput);
    let target = &input.target;
    let bindings: Vec<_> = input
        .fns
        .iter()
        .map(|fn_name| {
            let binding_fn = format_ident!("{}_binding", fn_name);
            quote! {
                #binding_fn(::otter_vm::NativeBindingTarget::#target)
            }
        })
        .collect();

    quote! {
        vec![#(#bindings),*]
    }
    .into()
}

#[proc_macro]
pub fn burrow(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as BurrowInput);
    let descriptors: Vec<_> = input
        .fns
        .iter()
        .map(|fn_name| {
            let descriptor_fn = format_ident!("{}_descriptor", fn_name);
            quote! {
                #descriptor_fn()
            }
        })
        .collect();

    quote! {
        vec![#(#descriptors),*]
    }
    .into()
}

fn expand_lodge_module(input: LodgeInput) -> TokenStream {
    let input_name = input.name.to_string();
    let struct_name = format_ident!("{}", to_pascal_case(&input_name));
    let entries_fn = format_ident!("{}_entries", input.name);
    let alloc_fn = format_ident!("{}_alloc_exported_function", input.name);
    let install_fn = format_ident!("{}_install_export", input.name);
    let default = input.default.clone();

    let specifiers: Vec<_> = input.module_specifiers.iter().collect();

    let function_exports: Vec<_> = input
        .functions
        .iter()
        .map(|export| {
            let export_name = &export.export_name;
            let descriptor_fn = format_ident!("{}_descriptor", export.source);
            let js_name = export
                .js_name
                .as_ref()
                .map(|value| quote! { Some(#value) })
                .unwrap_or_else(|| quote! { None });

            quote! {
                {
                    let handle = #alloc_fn(runtime, #export_name, #descriptor_fn(), #js_name)?;
                    let value = ::otter_runtime::RegisterValue::from_object_handle(handle.0);
                    #install_fn(runtime, namespace, #export_name, value)?;
                    if let Some(default_object) = default_object {
                        #install_fn(runtime, default_object, #export_name, value)?;
                    }
                }
            }
        })
        .collect();

    let value_exports: Vec<_> = input
        .values
        .iter()
        .map(|export| {
            let export_name = &export.export_name;
            let expr = &export.expr;

            quote! {
                {
                    let value: ::otter_runtime::RegisterValue = { #expr };
                    #install_fn(runtime, namespace, #export_name, value)?;
                    if let Some(default_object) = default_object {
                        #install_fn(runtime, default_object, #export_name, value)?;
                    }
                }
            }
        })
        .collect();

    let default_setup = match default.clone() {
        Some(LodgeDefault::Object) => quote! {
            let default_object = Some(runtime.alloc_object());
            let default_value: Option<::otter_runtime::RegisterValue> = None;
        },
        Some(LodgeDefault::Function(function)) => {
            let descriptor_fn = format_ident!("{}_descriptor", function.source);
            let js_name = function
                .js_name
                .as_ref()
                .map(|value| quote! { Some(#value) })
                .unwrap_or_else(|| quote! { None });

            quote! {
                let default_object: Option<::otter_runtime::ObjectHandle> = None;
                let default_value = Some(::otter_runtime::RegisterValue::from_object_handle(
                    #alloc_fn(runtime, "default", #descriptor_fn(), #js_name)?.0,
                ));
            }
        }
        Some(LodgeDefault::Value(expr)) => quote! {
            let default_object: Option<::otter_runtime::ObjectHandle> = None;
            let default_value = Some({
                let value: ::otter_runtime::RegisterValue = { #expr };
                value
            });
        },
        None => quote! {
            let default_object: Option<::otter_runtime::ObjectHandle> = None;
            let default_value: Option<::otter_runtime::RegisterValue> = None;
        },
    };

    let finalize_default = match default {
        Some(LodgeDefault::Object) => quote! {
            if let Some(default_object) = default_object {
                #install_fn(
                    runtime,
                    namespace,
                    "default",
                    ::otter_runtime::RegisterValue::from_object_handle(default_object.0),
                )?;
            }
        },
        _ => quote! {
            if let Some(default_value) = default_value {
                #install_fn(runtime, namespace, "default", default_value)?;
            }
        },
    };

    quote! {
        #[derive(Debug, Clone, Copy, Default)]
        pub(crate) struct #struct_name;

        fn #alloc_fn(
            runtime: &mut ::otter_runtime::RuntimeState,
            export_name: &str,
            descriptor: ::otter_vm::NativeFunctionDescriptor,
            js_name_override: Option<&str>,
        ) -> Result<::otter_runtime::ObjectHandle, String> {
            let function_name = js_name_override.unwrap_or(descriptor.js_name());
            let callback = *descriptor.callback();
            let function_descriptor = match (descriptor.slot_kind(), descriptor.entrypoint_kind()) {
                (::otter_vm::NativeSlotKind::Method, ::otter_vm::NativeEntrypointKind::Sync) => {
                    ::otter_vm::NativeFunctionDescriptor::method(
                        function_name,
                        descriptor.length(),
                        callback,
                    )
                }
                (::otter_vm::NativeSlotKind::Method, ::otter_vm::NativeEntrypointKind::Async) => {
                    ::otter_vm::NativeFunctionDescriptor::async_method(
                        function_name,
                        descriptor.length(),
                        callback,
                    )
                }
                (slot_kind, _) => {
                    return Err(format!(
                        "module export '{export_name}' must use method metadata, got {slot_kind:?}"
                    ));
                }
            };

            runtime
                .alloc_host_function_from_descriptor(function_descriptor)
                .map_err(|error| {
                    format!("failed to allocate function export '{export_name}': {error}")
                })
        }

        fn #install_fn(
            runtime: &mut ::otter_runtime::RuntimeState,
            target: ::otter_runtime::ObjectHandle,
            export_name: &str,
            value: ::otter_runtime::RegisterValue,
        ) -> Result<(), String> {
            let property = runtime.intern_property_name(export_name);
            runtime
                .objects_mut()
                .set_property(target, property, value)
                .map(|_| ())
                .map_err(|error| format!("failed to install module export '{export_name}': {error:?}"))
        }

        impl ::otter_runtime::HostedNativeModuleLoader for #struct_name {
            fn load(
                &self,
                runtime: &mut ::otter_runtime::RuntimeState,
            ) -> Result<::otter_runtime::HostedNativeModule, String> {
                let namespace = runtime.alloc_object();
                #default_setup
                #(#function_exports)*
                #(#value_exports)*
                #finalize_default
                Ok(::otter_runtime::HostedNativeModule::Esm(namespace))
            }
        }

        pub(crate) fn #entries_fn() -> Vec<::otter_runtime::HostedExtensionModule> {
            let loader: ::std::sync::Arc<dyn ::otter_runtime::HostedNativeModuleLoader> =
                ::std::sync::Arc::new(#struct_name);
            vec![
                #(
                    ::otter_runtime::HostedExtensionModule {
                        specifier: #specifiers.to_string(),
                        loader: loader.clone(),
                    }
                ),*
            ]
        }
    }
    .into()
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
        return expand_active_js_class_impl(i);
    }

    syn::Error::new(
        proc_macro2::Span::call_site(),
        "Expected struct or impl block",
    )
    .to_compile_error()
    .into()
}

/// Mark a struct or impl block as a JavaScript namespace in the active VM.
#[proc_macro_attribute]
pub fn js_namespace(attr: TokenStream, item: TokenStream) -> TokenStream {
    let args = parse_macro_input!(attr as JsClassArgs);

    let item_clone = item.clone();
    if let Ok(s) = syn::parse::<ItemStruct>(item_clone) {
        return js_namespace::expand_js_namespace_struct(s, args);
    }

    let item_clone = item.clone();
    if let Ok(i) = syn::parse::<ItemImpl>(item_clone) {
        return js_namespace::expand_js_namespace_impl(i);
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

        if let Some(ident) = &field.ident
            && !is_skip
        {
            if is_readonly {
                js_readonly.push(ident.clone());
            } else {
                js_properties.push(ident.clone());
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum JsMemberKind {
    Constructor,
    Method,
    Static,
    Getter,
    Setter,
}

impl JsMemberKind {
    fn default_length(self) -> u32 {
        match self {
            Self::Constructor | Self::Method | Self::Static | Self::Getter => 0,
            Self::Setter => 1,
        }
    }
}

struct JsMemberAttr {
    kind: JsMemberKind,
    name: Option<String>,
    length: Option<u32>,
}

fn parse_js_member_attr(attr: &syn::Attribute) -> syn::Result<Option<JsMemberAttr>> {
    let mut parsed = if attr.path().is_ident("js_constructor") {
        Some(JsMemberAttr {
            kind: JsMemberKind::Constructor,
            name: None,
            length: None,
        })
    } else if attr.path().is_ident("js_method") {
        Some(JsMemberAttr {
            kind: JsMemberKind::Method,
            name: None,
            length: None,
        })
    } else if attr.path().is_ident("js_static") {
        Some(JsMemberAttr {
            kind: JsMemberKind::Static,
            name: None,
            length: None,
        })
    } else if attr.path().is_ident("js_getter") {
        Some(JsMemberAttr {
            kind: JsMemberKind::Getter,
            name: None,
            length: None,
        })
    } else if attr.path().is_ident("js_setter") {
        Some(JsMemberAttr {
            kind: JsMemberKind::Setter,
            name: None,
            length: None,
        })
    } else {
        None
    };

    let Some(ref mut parsed_attr) = parsed else {
        return Ok(None);
    };

    if let syn::Meta::List(list) = &attr.meta {
        syn::parse::Parser::parse2(
            |input: ParseStream| {
                while !input.is_empty() {
                    let ident: Ident = input.parse()?;
                    match ident.to_string().as_str() {
                        "name" => {
                            input.parse::<Token![=]>()?;
                            let lit: LitStr = input.parse()?;
                            parsed_attr.name = Some(lit.value());
                        }
                        "length" => {
                            input.parse::<Token![=]>()?;
                            let lit: LitInt = input.parse()?;
                            parsed_attr.length = Some(lit.base10_parse()?);
                        }
                        other => {
                            return Err(syn::Error::new_spanned(
                                ident,
                                format!(
                                    "Unknown js_class member option '{other}'. Expected name or length."
                                ),
                            ));
                        }
                    }

                    if input.peek(Token![,]) {
                        input.parse::<Token![,]>()?;
                    }
                }
                Ok(())
            },
            list.tokens.clone(),
        )?;
    }

    Ok(parsed)
}

fn is_type_named(ty: &Type, expected: &str) -> bool {
    match ty {
        Type::Path(type_path) => type_path
            .path
            .segments
            .last()
            .is_some_and(|segment| segment.ident == expected),
        _ => false,
    }
}

fn is_register_value_ref(arg: &FnArg) -> bool {
    let FnArg::Typed(pat_type) = arg else {
        return false;
    };

    if let Type::Reference(type_ref) = &*pat_type.ty {
        return is_type_named(&type_ref.elem, "RegisterValue");
    }

    false
}

fn is_register_value_slice(arg: &FnArg) -> bool {
    let FnArg::Typed(pat_type) = arg else {
        return false;
    };

    if let Type::Reference(type_ref) = &*pat_type.ty
        && let Type::Slice(type_slice) = &*type_ref.elem
    {
        return is_type_named(&type_slice.elem, "RegisterValue");
    }

    false
}

fn is_runtime_state_ref(arg: &FnArg) -> bool {
    let FnArg::Typed(pat_type) = arg else {
        return false;
    };

    if let Type::Reference(type_ref) = &*pat_type.ty {
        return type_ref.mutability.is_some() && is_type_named(&type_ref.elem, "RuntimeState");
    }

    false
}

fn is_active_js_class_method(method: &syn::ImplItemFn) -> bool {
    let params: Vec<_> = method.sig.inputs.iter().collect();
    params.len() == 3
        && is_register_value_ref(params[0])
        && is_register_value_slice(params[1])
        && is_runtime_state_ref(params[2])
}

fn expand_active_js_class_impl(input: ItemImpl) -> TokenStream {
    let self_ty = &input.self_ty;
    let mut errors = Vec::new();

    let mut constructors = Vec::new();
    let mut methods = Vec::new();
    let mut static_methods = Vec::new();
    let mut js_getters = Vec::new();
    let mut js_setters = Vec::new();

    struct DescriptorInfo {
        rust_ident: Ident,
        js_name: String,
        length: u32,
        kind: JsMemberKind,
    }
    let mut descriptor_members: Vec<DescriptorInfo> = Vec::new();
    let mut descriptor_constructor: Option<DescriptorInfo> = None;

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
                                    "Expected at most one js_class member attribute per method.",
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

            let js_name = member_attr.name.unwrap_or_else(|| rust_name.clone());
            let length = member_attr
                .length
                .unwrap_or_else(|| member_attr.kind.default_length());

            match member_attr.kind {
                JsMemberKind::Constructor => constructors.push(rust_name.clone()),
                JsMemberKind::Method => methods.push(rust_name.clone()),
                JsMemberKind::Static => static_methods.push(rust_name.clone()),
                JsMemberKind::Getter => js_getters.push(rust_name.clone()),
                JsMemberKind::Setter => js_setters.push(rust_name.clone()),
            }

            if !is_active_js_class_method(method) {
                errors.push(
                    syn::Error::new_spanned(
                        &method.sig.ident,
                        "js_class only supports active runtime methods with signature fn(&RegisterValue, &[RegisterValue], &mut RuntimeState) -> Result<RegisterValue, VmNativeCallError>.",
                    )
                    .to_compile_error(),
                );
                continue;
            }

            let info = DescriptorInfo {
                rust_ident: rust_ident.clone(),
                js_name,
                length,
                kind: member_attr.kind,
            };

            if info.kind == JsMemberKind::Constructor {
                if descriptor_constructor.is_some() {
                    errors.push(
                        syn::Error::new_spanned(
                            &method.sig.ident,
                            "Expected at most one js_class constructor for descriptor metadata.",
                        )
                        .to_compile_error(),
                    );
                } else {
                    descriptor_constructor = Some(info);
                }
            } else {
                descriptor_members.push(info);
            }
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
        .chain(descriptor_constructor.iter())
        .map(|info| {
            let descriptor_fn_name = format_ident!("{}_descriptor", info.rust_ident);
            let rust_ident = &info.rust_ident;
            let js_name = &info.js_name;
            let length = info.length;

            let descriptor_ctor = match info.kind {
                JsMemberKind::Constructor => {
                    quote! {
                        ::otter_vm::NativeFunctionDescriptor::constructor(
                            #js_name,
                            #length as u16,
                            callback,
                        )
                    }
                }
                JsMemberKind::Method | JsMemberKind::Static => {
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
            };

            quote! {
                /// Descriptor for this js_class member.
                pub fn #descriptor_fn_name() -> ::otter_vm::NativeFunctionDescriptor {
                    let callback = Self::#rust_ident as ::otter_vm::VmNativeFunction;
                    #descriptor_ctor
                }
            }
        })
        .collect();

    let class_descriptor_fn = if descriptor_constructor.is_some() || !descriptor_members.is_empty()
    {
        let constructor_binding = descriptor_constructor.as_ref().map(|info| {
            let descriptor_fn_name = format_ident!("{}_descriptor", info.rust_ident);
            quote! {
                descriptor = descriptor.with_constructor(Self::#descriptor_fn_name());
            }
        });

        let binding_pushes: Vec<_> = descriptor_members
            .iter()
            .map(|info| {
                let descriptor_fn_name = format_ident!("{}_descriptor", info.rust_ident);
                let target = match info.kind {
                    JsMemberKind::Static => quote!(::otter_vm::NativeBindingTarget::Constructor),
                    JsMemberKind::Method | JsMemberKind::Getter | JsMemberKind::Setter => {
                        quote!(::otter_vm::NativeBindingTarget::Prototype)
                    }
                    JsMemberKind::Constructor => {
                        quote!(::otter_vm::NativeBindingTarget::Constructor)
                    }
                };

                quote! {
                    descriptor = descriptor.with_binding(::otter_vm::NativeBindingDescriptor::new(
                        #target,
                        Self::#descriptor_fn_name(),
                    ));
                }
            })
            .collect();

        quote! {
            /// Aggregate class descriptor emitted by #[js_class].
            pub fn js_class_descriptor() -> ::otter_vm::JsClassDescriptor {
                let mut descriptor = ::otter_vm::JsClassDescriptor::new(Self::JS_CLASS_NAME);
                #constructor_binding
                #(#binding_pushes)*
                descriptor
            }
        }
    } else {
        quote! {}
    };

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

            #(#descriptor_fns)*
            #class_descriptor_fn
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
