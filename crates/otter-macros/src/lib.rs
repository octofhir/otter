//! # Otter Macros - Proc-macros for the Otter Runtime
//!
//! This crate provides procedural macros for creating native functions callable from JavaScript
//! in the Otter runtime.
//!
//! ## The `#[dive]` Attribute
//!
//! The `#[dive]` attribute marks a Rust function as callable from JavaScript. The name comes from
//! the way otters dive for fish - our functions "dive" into native code to fetch results.
//!
//! ### Modes
//!
//! - `#[dive]` or `#[dive(swift)]` - Synchronous function, returns value directly
//! - `#[dive(deep)]` - Async function, returns a Promise that resolves when complete
//!
//! ### Example
//!
//! ```ignore
//! use otter_macros::dive;
//!
//! #[dive(swift)]  // Quick synchronous operation
//! fn add(a: i32, b: i32) -> i32 {
//!     a + b
//! }
//!
//! #[dive(deep)]  // Async operation - returns Promise
//! async fn fetch_data(url: String) -> Result<String, Error> {
//!     // ... async implementation
//! }
//! ```
//!
//! ## Otter Terminology
//!
//! | Term | Meaning |
//! |------|---------|
//! | **dive** | A native function (otters dive for fish) |
//! | **swift** | Fast synchronous dive |
//! | **deep** | Async dive that goes "deeper" and returns a Promise |
//!
//! ## Integration with Extension System
//!
//! The `#[dive]` macro generates code compatible with the otter-runtime extension
//! system. Each dive function generates:
//!
//! 1. The original function (unchanged)
//! 2. A `{name}_dive_decl()` function returning `OpDecl` for registration
//!
//! ```ignore
//! // This:
//! #[dive(swift)]
//! fn add(a: i32, b: i32) -> i32 { a + b }
//!
//! // Generates:
//! fn add(a: i32, b: i32) -> i32 { a + b }
//!
//! pub fn add_dive_decl() -> otter_runtime::extension::OpDecl {
//!     otter_runtime::extension::op_sync("add", |_ctx, args| {
//!         let a: i32 = serde_json::from_value(args[0].clone())?;
//!         let b: i32 = serde_json::from_value(args[1].clone())?;
//!         let result = add(a, b);
//!         Ok(serde_json::to_value(result)?)
//!     })
//! }
//! ```

use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::{
    parse::{Parse, ParseStream},
    parse_macro_input, FnArg, Ident, ItemFn, Pat, ReturnType, Type,
};

/// Dive mode - how the function behaves
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiveMode {
    /// Quick synchronous operation (default)
    Swift,
    /// Deep async operation - returns Promise
    Deep,
}

impl Default for DiveMode {
    fn default() -> Self {
        DiveMode::Swift
    }
}

/// Arguments to the #[dive] attribute
struct DiveArgs {
    mode: DiveMode,
}

impl Default for DiveArgs {
    fn default() -> Self {
        Self {
            mode: DiveMode::default(),
        }
    }
}

impl Parse for DiveArgs {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        if input.is_empty() {
            return Ok(Self::default());
        }

        let mode_ident: Ident = input.parse()?;
        let mode = match mode_ident.to_string().as_str() {
            "swift" => DiveMode::Swift,
            "deep" => DiveMode::Deep,
            other => {
                return Err(syn::Error::new_spanned(
                    mode_ident,
                    format!(
                        "Unknown dive mode '{}'. Expected 'swift' (sync) or 'deep' (async).",
                        other
                    ),
                ))
            }
        };

        Ok(Self { mode })
    }
}

/// Extract parameter info from function arguments
struct ParamInfo {
    name: Ident,
    ty: Type,
}

fn extract_params(func: &ItemFn) -> Vec<ParamInfo> {
    func.sig
        .inputs
        .iter()
        .filter_map(|arg| {
            if let FnArg::Typed(pat_type) = arg {
                if let Pat::Ident(pat_ident) = &*pat_type.pat {
                    return Some(ParamInfo {
                        name: pat_ident.ident.clone(),
                        ty: (*pat_type.ty).clone(),
                    });
                }
            }
            None
        })
        .collect()
}

/// Extract the inner type from Result<T, E>
fn extract_result_ok_type(ret: &ReturnType) -> Option<&Type> {
    if let ReturnType::Type(_, ty) = ret {
        if let Type::Path(type_path) = ty.as_ref() {
            if let Some(segment) = type_path.path.segments.last() {
                if segment.ident == "Result" {
                    if let syn::PathArguments::AngleBracketed(args) = &segment.arguments {
                        if let Some(syn::GenericArgument::Type(inner_ty)) = args.args.first() {
                            return Some(inner_ty);
                        }
                    }
                }
            }
        }
    }
    None
}

/// Check if return type is Result
fn is_result_type(ret: &ReturnType) -> bool {
    extract_result_ok_type(ret).is_some()
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
/// ## Supported Types
///
/// Arguments and return types must implement `serde::Serialize` and `serde::Deserialize`.
/// Common types that work out of the box:
/// - Primitives: `i32`, `i64`, `f64`, `bool`, `String`
/// - Collections: `Vec<T>`, `HashMap<K, V>`
/// - Options: `Option<T>`
/// - Custom types with `#[derive(Serialize, Deserialize)]`
///
/// ## Return Types
///
/// - `T` - Returns `Ok(serde_json::to_value(result)?)`
/// - `Result<T, E>` - Maps error to JSON error, returns `Ok(serde_json::to_value(ok_value)?)`
///
/// ## Example
///
/// ```ignore
/// use otter_macros::dive;
/// use serde::{Deserialize, Serialize};
///
/// #[dive(swift)]
/// fn add(a: i32, b: i32) -> i32 {
///     a + b
/// }
///
/// #[dive(swift)]
/// fn greet(name: String) -> String {
///     format!("Hello, {}!", name)
/// }
///
/// #[dive(deep)]
/// async fn fetch_json(url: String) -> Result<serde_json::Value, anyhow::Error> {
///     let resp = reqwest::get(&url).await?;
///     Ok(resp.json().await?)
/// }
/// ```
///
/// ## Generated Code
///
/// The macro generates:
/// 1. The original function (unchanged)
/// 2. A `{name}_dive_decl()` function returning `OpDecl` for registration
#[proc_macro_attribute]
pub fn dive(attr: TokenStream, item: TokenStream) -> TokenStream {
    let args = parse_macro_input!(attr as DiveArgs);
    let func = parse_macro_input!(item as ItemFn);

    let func_name = &func.sig.ident;
    let func_name_str = func_name.to_string();
    let func_vis = &func.vis;
    let decl_fn_name = format_ident!("{}_dive_decl", func_name);

    let params = extract_params(&func);
    let is_async = func.sig.asyncness.is_some();
    let returns_result = is_result_type(&func.sig.output);

    // Validate mode matches async-ness
    if args.mode == DiveMode::Deep && !is_async {
        return syn::Error::new_spanned(
            &func.sig,
            "#[dive(deep)] requires an async function. Use `async fn` or switch to #[dive(swift)].",
        )
        .to_compile_error()
        .into();
    }

    if args.mode == DiveMode::Swift && is_async {
        return syn::Error::new_spanned(
            &func.sig,
            "#[dive(swift)] cannot be used with async functions. Use #[dive(deep)] for async.",
        )
        .to_compile_error()
        .into();
    }

    // Generate argument extraction code
    let arg_extractions: Vec<_> = params
        .iter()
        .enumerate()
        .map(|(i, param)| {
            let name = &param.name;
            let ty = &param.ty;
            quote! {
                let #name: #ty = serde_json::from_value(
                    args.get(#i).cloned().unwrap_or(serde_json::Value::Null)
                ).map_err(|e| otter_runtime::error::JscError::internal(
                    format!("Failed to deserialize argument {}: {}", #i, e)
                ))?;
            }
        })
        .collect();

    let param_names: Vec<_> = params.iter().map(|p| &p.name).collect();

    // Generate result conversion based on whether it returns Result or not
    let result_conversion = if returns_result {
        quote! {
            let result = result.map_err(|e| otter_runtime::error::JscError::internal(
                format!("{}", e)
            ))?;
            Ok(serde_json::to_value(result)?)
        }
    } else {
        quote! {
            Ok(serde_json::to_value(result)?)
        }
    };

    // Generate the OpDecl function based on mode
    let decl_fn_body = match args.mode {
        DiveMode::Swift => {
            quote! {
                /// Returns an OpDecl for this dive function.
                /// Auto-generated by #[dive(swift)]
                #func_vis fn #decl_fn_name() -> otter_runtime::extension::OpDecl {
                    otter_runtime::extension::op_sync(#func_name_str, |_ctx, args| {
                        #(#arg_extractions)*
                        let result = #func_name(#(#param_names),*);
                        #result_conversion
                    })
                }
            }
        }
        DiveMode::Deep => {
            quote! {
                /// Returns an OpDecl for this dive function.
                /// Auto-generated by #[dive(deep)]
                #func_vis fn #decl_fn_name() -> otter_runtime::extension::OpDecl {
                    otter_runtime::extension::op_async(#func_name_str, |_ctx, args| {
                        async move {
                            #(#arg_extractions)*
                            let result = #func_name(#(#param_names),*).await;
                            #result_conversion
                        }
                    })
                }
            }
        }
    };

    // Combine original function with decl function
    let expanded = quote! {
        #func

        #decl_fn_body
    };

    expanded.into()
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
///
/// // Generates:
/// // pub fn path_ops_dive_decls() -> Vec<OpDecl> { ... }
/// ```
#[proc_macro_attribute]
pub fn dive_module(_attr: TokenStream, item: TokenStream) -> TokenStream {
    // For now, just pass through - we can implement collection later
    // This is a placeholder for future enhancement
    item
}

#[cfg(test)]
mod tests {
    // Proc-macro tests require integration tests or trybuild
}
