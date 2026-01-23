//! # Otter Macros
//!
//! Proc macros for defining operations and extensions in the Otter VM.
//!
//! ## Otter Terminology
//!
//! | Term | Meaning |
//! |------|---------|
//! | **dive** | A native function (otters dive for fish) |
//! | **swift** | Fast synchronous dive (default) |
//! | **deep** | Async dive that goes "deeper" and returns a Promise |
//!
//! ## Example
//!
//! ```ignore
//! use otter_macros::dive;
//!
//! #[dive(swift)]
//! fn add(a: i32, b: i32) -> i32 {
//!     a + b
//! }
//!
//! #[dive(deep)]
//! async fn fetch_data(url: String) -> Result<String, Error> {
//!     // async implementation
//! }
//! ```

use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::{
    FnArg, Ident, ItemFn, ItemImpl, ItemStruct, LitStr, Pat, ReturnType, Token, Type,
    parse::{Parse, ParseStream},
    parse_macro_input,
};

// =============================================================================
// #[dive] macro
// =============================================================================

/// Dive mode - how the function behaves
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum DiveMode {
    /// Quick synchronous operation (default)
    #[default]
    Swift,
    /// Deep async operation - returns Promise
    Deep,
}

/// Arguments to the #[dive] attribute
#[derive(Default)]
struct DiveArgs {
    mode: DiveMode,
    /// Custom operation name
    name: Option<String>,
}

impl Parse for DiveArgs {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        if input.is_empty() {
            return Ok(Self::default());
        }

        let mut mode = DiveMode::default();
        let mut name = None;

        // Parse first token - could be mode (swift/deep) or name
        if input.peek(Ident) {
            let ident: Ident = input.parse()?;
            match ident.to_string().as_str() {
                "swift" => mode = DiveMode::Swift,
                "deep" => mode = DiveMode::Deep,
                "name" => {
                    input.parse::<Token![=]>()?;
                    let lit: LitStr = input.parse()?;
                    name = Some(lit.value());
                }
                other => {
                    return Err(syn::Error::new_spanned(
                        ident,
                        format!(
                            "Unknown dive argument '{}'. Expected 'swift', 'deep', or 'name'.",
                            other
                        ),
                    ));
                }
            }
        }

        // Parse remaining options
        while input.peek(Token![,]) {
            input.parse::<Token![,]>()?;
            if input.is_empty() {
                break;
            }
            let key: Ident = input.parse()?;
            if key == "name" {
                input.parse::<Token![=]>()?;
                let lit: LitStr = input.parse()?;
                name = Some(lit.value());
            } else {
                return Err(syn::Error::new_spanned(
                    &key,
                    format!("Unknown option '{}'. Expected 'name'.", key),
                ));
            }
        }

        Ok(Self { mode, name })
    }
}

/// Marks a function as callable from JavaScript.
///
/// Like an otter diving for fish - goes deep into native code and surfaces with results.
///
/// ## Modes
///
/// - `#[dive]` or `#[dive(swift)]` - Sync function (quick dive)
/// - `#[dive(deep)]` - Async function, returns Promise (deep dive)
///
/// ## Options
///
/// - `name = "custom_name"` - Custom operation name (default: function name)
///
/// ## Supported Types
///
/// Arguments and return types must implement `serde::Serialize` and `serde::Deserialize`.
/// Common types that work out of the box:
/// - Primitives: `i32`, `i64`, `f64`, `bool`, `String`
/// - Collections: `Vec<T>`, `HashMap<K, V>`
/// - Options: `Option<T>`
/// - Custom types with `#[derive(Serialize, Deserialize)]`
///
/// ## Generated Code
///
/// The macro generates:
/// 1. The original function (unchanged)
/// 2. A `__otter_dive_{name}` wrapper function for runtime integration
/// 3. A `{name}` module with `NAME` and `IS_ASYNC` constants
///
/// ## Example
///
/// ```ignore
/// use otter_macros::dive;
///
/// #[dive(swift)]
/// fn add(a: i32, b: i32) -> i32 {
///     a + b
/// }
///
/// #[dive(swift, name = "custom_divide")]
/// fn divide(a: f64, b: f64) -> Result<f64, String> {
///     if b == 0.0 {
///         Err("Division by zero".to_string())
///     } else {
///         Ok(a / b)
///     }
/// }
///
/// #[dive(deep)]
/// async fn fetch_json(url: String) -> Result<serde_json::Value, String> {
///     // async implementation
/// }
/// ```
#[proc_macro_attribute]
pub fn dive(attr: TokenStream, item: TokenStream) -> TokenStream {
    let args = parse_macro_input!(attr as DiveArgs);
    let input = parse_macro_input!(item as ItemFn);

    let is_async = input.sig.asyncness.is_some();

    // Validate mode matches async-ness
    if args.mode == DiveMode::Deep && !is_async {
        return syn::Error::new_spanned(
            &input.sig,
            "#[dive(deep)] requires an async function. Use `async fn` or switch to #[dive(swift)].",
        )
        .to_compile_error()
        .into();
    }

    if args.mode == DiveMode::Swift && is_async {
        return syn::Error::new_spanned(
            &input.sig,
            "#[dive(swift)] cannot be used with async functions. Use #[dive(deep)] for async.",
        )
        .to_compile_error()
        .into();
    }

    expand_dive(input, args)
}

/// Expand the dive macro
fn expand_dive(input: ItemFn, args: DiveArgs) -> TokenStream {
    let fn_name = &input.sig.ident;
    let vis = &input.vis;
    let block = &input.block;
    let generics = &input.sig.generics;
    let output = &input.sig.output;
    let is_async = input.sig.asyncness.is_some();

    // Operation name (from attr or function name)
    let op_name = args.name.unwrap_or_else(|| fn_name.to_string());

    // Wrapper function name
    let wrapper_name = format_ident!("__otter_dive_{}", fn_name);

    // Parse arguments
    let params: Vec<_> = input.sig.inputs.iter().collect();

    // Generate argument extraction
    let (extractions, arg_names): (Vec<_>, Vec<_>) = params
        .iter()
        .enumerate()
        .filter_map(|(i, arg)| {
            if let FnArg::Typed(pat_type) = arg
                && let Pat::Ident(pat_ident) = &*pat_type.pat
            {
                let arg_name = &pat_ident.ident;
                let arg_type = &pat_type.ty;

                let extraction = quote! {
                    let #arg_name: #arg_type = args.get(#i)
                        .cloned()
                        .and_then(|v| serde_json::from_value(v).ok())
                        .ok_or_else(|| format!("Missing or invalid argument {}", #i))?;
                };

                return Some((extraction, arg_name.clone()));
            }
            None
        })
        .unzip();

    // Determine return type handling
    let is_result = is_result_type(output);

    let call_and_convert = if is_async {
        if is_result {
            quote! {
                let result = inner_fn(#(#arg_names),*).await;
                match result {
                    Ok(v) => Ok(serde_json::to_value(v).unwrap_or(serde_json::Value::Null)),
                    Err(e) => Err(format!("{}", e)),
                }
            }
        } else {
            quote! {
                let result = inner_fn(#(#arg_names),*).await;
                Ok(serde_json::to_value(result).unwrap_or(serde_json::Value::Null))
            }
        }
    } else if is_result {
        quote! {
            let result = inner_fn(#(#arg_names),*);
            match result {
                Ok(v) => Ok(serde_json::to_value(v).unwrap_or(serde_json::Value::Null)),
                Err(e) => Err(format!("{}", e)),
            }
        }
    } else {
        quote! {
            let result = inner_fn(#(#arg_names),*);
            Ok(serde_json::to_value(result).unwrap_or(serde_json::Value::Null))
        }
    };

    let expanded = if is_async {
        quote! {
            // Original function
            #input

            // Wrapper for runtime integration
            #vis fn #wrapper_name(
                args: &[serde_json::Value]
            ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<serde_json::Value, String>> + Send>> {
                let args = args.to_vec();
                Box::pin(async move {
                    #[allow(unused_variables)]
                    async fn inner_fn #generics (#(#params),*) #output #block

                    #(#extractions)*
                    #call_and_convert
                })
            }

            /// Operation descriptor
            #vis mod #fn_name {
                /// The operation name
                pub const NAME: &str = #op_name;
                /// Whether this is an async operation
                pub const IS_ASYNC: bool = true;
            }
        }
    } else {
        quote! {
            // Original function
            #input

            // Wrapper for runtime integration
            #vis fn #wrapper_name(
                args: &[serde_json::Value]
            ) -> Result<serde_json::Value, String> {
                #[allow(unused_variables)]
                fn inner_fn #generics (#(#params),*) #output #block

                #(#extractions)*
                #call_and_convert
            }

            /// Operation descriptor
            #vis mod #fn_name {
                /// The operation name
                pub const NAME: &str = #op_name;
                /// Whether this is an async operation
                pub const IS_ASYNC: bool = false;
            }
        }
    };

    TokenStream::from(expanded)
}

/// Check if return type is Result<T, E>
fn is_result_type(output: &ReturnType) -> bool {
    if let ReturnType::Type(_, ty) = output
        && let Type::Path(type_path) = ty.as_ref()
        && let Some(segment) = type_path.path.segments.last()
    {
        return segment.ident == "Result";
    }
    false
}

/// Collects dive functions in a module for easy registration.
///
/// This attribute is applied to a module to collect all `#[dive]` functions
/// and generate a function that returns all their OpDecls.
///
/// ## Example
///
/// ```ignore
/// #[dive_module]
/// mod path_ops {
///     use otter_macros::dive;
///
///     #[dive(swift)]
///     fn join(parts: Vec<String>) -> String { ... }
///
///     #[dive(swift)]
///     fn dirname(path: String) -> String { ... }
/// }
/// ```
#[proc_macro_attribute]
pub fn dive_module(_attr: TokenStream, item: TokenStream) -> TokenStream {
    // Pass through for now.
    // Full implementation will come later.
    item
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
/// This generates the necessary boilerplate for exposing a Rust
/// struct as a JavaScript class in the Otter VM.
///
/// ## Options
///
/// - `name = "ClassName"` - Custom JavaScript class name (default: struct name)
///
/// ## Field Attributes
///
/// - `#[js_readonly]` - Expose field as read-only property
/// - `#[js_skip]` - Don't expose this field to JavaScript
///
/// ## Method Attributes (on impl block)
///
/// - `#[js_constructor]` - Mark as constructor
/// - `#[js_method]` - Mark as instance method
/// - `#[js_static]` - Mark as static method
/// - `#[js_getter]` - Mark as property getter
/// - `#[js_setter]` - Mark as property setter
///
/// ## Example
///
/// ```ignore
/// use otter_macros::{js_class, js_constructor, js_method, js_getter};
///
/// #[js_class(name = "Counter")]
/// pub struct Counter {
///     #[js_readonly]
///     pub value: i32,
///
///     #[js_skip]
///     internal: String,
/// }
///
/// #[js_class]
/// impl Counter {
///     #[js_constructor]
///     pub fn new(initial: i32) -> Self {
///         Self { value: initial, internal: String::new() }
///     }
///
///     #[js_method]
///     pub fn increment(&mut self) {
///         self.value += 1;
///     }
///
///     #[js_getter]
///     pub fn doubled(&self) -> i32 {
///         self.value * 2
///     }
/// }
/// ```
#[proc_macro_attribute]
pub fn js_class(attr: TokenStream, item: TokenStream) -> TokenStream {
    let args = parse_macro_input!(attr as JsClassArgs);

    // Try to parse as struct first, then as impl
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

    // Collect fields for JS exposure and strip js_* attributes
    let mut js_properties = Vec::new();
    let mut js_readonly = Vec::new();
    let mut cleaned_fields = Vec::new();

    for field in input.fields.iter() {
        // Check for js_* attributes
        let is_skip = field.attrs.iter().any(|a| a.path().is_ident("js_skip"));
        let is_readonly = field.attrs.iter().any(|a| a.path().is_ident("js_readonly"));

        // Remove js_* attributes from the field
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

        // Build cleaned field
        let field_vis = &field.vis;
        let field_ident = &field.ident;
        let field_ty = &field.ty;
        cleaned_fields.push(quote! {
            #(#cleaned_attrs)*
            #field_vis #field_ident: #field_ty
        });
    }

    // Generate property getters
    let getters: Vec<_> = js_properties
        .iter()
        .chain(js_readonly.iter())
        .map(|name| {
            let getter_name = format_ident!("js_get_{}", name);
            quote! {
                /// Get property value as JSON
                pub fn #getter_name(&self) -> serde_json::Value {
                    serde_json::to_value(&self.#name).unwrap_or(serde_json::Value::Null)
                }
            }
        })
        .collect();

    // Generate property setters (only for non-readonly)
    let setters: Vec<_> = js_properties
        .iter()
        .map(|name| {
            let setter_name = format_ident!("js_set_{}", name);
            quote! {
                /// Set property value from JSON
                pub fn #setter_name(&mut self, value: serde_json::Value) {
                    if let Ok(v) = serde_json::from_value(value) {
                        self.#name = v;
                    }
                }
            }
        })
        .collect();

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

            #(#getters)*
            #(#setters)*
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

// Note: js_readonly and js_skip are NOT proc-macro attributes.
// They are inert attributes parsed by #[js_class] and stripped from output.
// Use them as: #[js_readonly] or #[js_skip] on struct fields.

#[cfg(test)]
mod tests {
    // Proc-macro tests require integration tests or trybuild
}
