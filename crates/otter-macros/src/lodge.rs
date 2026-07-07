//! `lodge!` proc macro — hosted module installer generator.
//!
//! Otters raise families in **lodges**: the macro builds the
//! family residence for one hosted module (`otter:kv`,
//! `otter:sql`, `otter:ffi`, `node:url`, …). It emits the
//! installer fn that `HostedModule::new(<prefix>:<name>,
//! HostedModuleInstall::new(install_fn))` consumes, plus a
//! `pub static <UPPER>_HOSTED_MODULE: HostedModule` row callers
//! drop into their `HOSTED_MODULES` array.
//!
//! # Surface
//!
//! Plain module — static export fns, no captures:
//!
//! ```rust,ignore
//! lodge! {
//!     prefix = "otter",
//!     name = "math",
//!     exports = {
//!         "add" / 2 => add_fn,
//!         "mul" / 2 => mul_fn,
//!     },
//! }
//! ```
//!
//! Bare module — static export fns under an exact module specifier:
//!
//! ```rust,ignore
//! lodge! {
//!     specifier = "otter",
//!     name = "otter",
//!     exports = {
//!         "serve" / 1 => serve,
//!     },
//! }
//! ```
//!
//! Capability-aware module — each export receives a borrowed
//! `CapabilitySet` snapshot captured at install time:
//!
//! ```rust,ignore
//! lodge! {
//!     prefix = "otter",
//!     name = "kv",
//!     capabilities = true,
//!     exports = {
//!         "openKv" / 1 => open_kv,
//!         "kv"     / 1 => open_kv,
//!     },
//! }
//!
//! fn open_kv(
//!     ctx: &mut NativeCtx<'_>,
//!     args: &[Value],
//!     caps: &CapabilitySet,
//! ) -> Result<Value, NativeError> { ... }
//! ```
//!
//! # Generated symbols
//!
//! - `pub fn install_<name>_module(ctx: &mut HostedModuleCtx<'_>)
//!   -> Result<(), String>` — the runtime installer.
//! - `pub static <UPPER>_HOSTED_MODULE: HostedModule` — a row
//!   ready to drop into `HOSTED_MODULES`.
//!
//! # See also
//! - [`crate::holt`] — namespace intrinsic generator (sibling
//!   surface for `globalThis.<NAME>`).
//! - [`crate::couch`] — class intrinsic generator (sibling
//!   surface for callable constructors).

use proc_macro::TokenStream;
use proc_macro2::Span;
use quote::{format_ident, quote};
use std::collections::BTreeSet;
use syn::parse::{Parse, ParseStream};
use syn::{Ident, LitBool, LitInt, LitStr, Path, Result, Token, braced, parse_macro_input};

/// Parsed `"name" / length => path` row.
pub(crate) struct LodgeExport {
    pub(crate) js_name: LitStr,
    pub(crate) length: u8,
    pub(crate) call: Path,
}

impl Parse for LodgeExport {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let js_name: LitStr = input.parse()?;
        input.parse::<Token![/]>()?;
        let length_lit: LitInt = input.parse()?;
        let length = length_lit.base10_parse::<u8>()?;
        input.parse::<Token![=>]>()?;
        let call: Path = input.parse()?;
        Ok(Self {
            js_name,
            length,
            call,
        })
    }
}

pub(crate) struct LodgeInput {
    pub(crate) prefix: Option<LitStr>,
    pub(crate) specifier: Option<LitStr>,
    pub(crate) name: LitStr,
    pub(crate) capabilities: bool,
    pub(crate) exports: Vec<LodgeExport>,
    pub(crate) install_ident: Ident,
    pub(crate) static_ident: Ident,
}

impl Parse for LodgeInput {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let mut prefix: Option<LitStr> = None;
        let mut specifier: Option<LitStr> = None;
        let mut name: Option<LitStr> = None;
        let mut capabilities = false;
        let mut exports: Vec<LodgeExport> = Vec::new();
        let mut install_override: Option<Ident> = None;
        let mut static_override: Option<Ident> = None;

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
                "install" => install_override = Some(input.parse()?),
                "module_static" => static_override = Some(input.parse()?),
                "exports" => {
                    let body;
                    braced!(body in input);
                    while !body.is_empty() {
                        exports.push(body.parse()?);
                        if body.peek(Token![,]) {
                            body.parse::<Token![,]>()?;
                        }
                    }
                }
                other => {
                    return Err(syn::Error::new(
                        key.span(),
                        format!(
                            "unknown `lodge!` field `{other}` — expected `prefix`, `specifier`, \
                             `name`, `capabilities`, `exports`, `install`, or `module_static`"
                        ),
                    ));
                }
            }
            if input.peek(Token![,]) {
                input.parse::<Token![,]>()?;
            }
        }

        if prefix.is_none() && specifier.is_none() {
            return Err(syn::Error::new(
                Span::call_site(),
                "lodge!: missing `prefix = \"...\"` or `specifier = \"...\"`",
            ));
        }
        let name = name.ok_or_else(|| {
            syn::Error::new(Span::call_site(), "lodge!: missing `name = \"...\"`")
        })?;
        let name_str = name.value();
        let install_ident = install_override.unwrap_or_else(|| {
            format_ident!(
                "install_{}_module",
                sanitize_ident(&name_str),
                span = name.span()
            )
        });
        let static_ident = static_override.unwrap_or_else(|| {
            let upper = name_str.to_ascii_uppercase();
            format_ident!(
                "{}_HOSTED_MODULE",
                sanitize_ident(&upper),
                span = name.span()
            )
        });

        Ok(Self {
            prefix,
            specifier,
            name,
            capabilities,
            exports,
            install_ident,
            static_ident,
        })
    }
}

fn sanitize_ident(raw: &str) -> String {
    raw.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

pub(crate) fn expand(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as LodgeInput);
    let LodgeInput {
        prefix,
        specifier,
        name,
        capabilities,
        exports,
        install_ident,
        static_ident,
    } = input;

    let mut seen = BTreeSet::new();
    for ex in &exports {
        if !seen.insert(ex.js_name.value()) {
            return syn::Error::new_spanned(
                &ex.js_name,
                format!("lodge!: duplicate export name `{}`", ex.js_name.value()),
            )
            .to_compile_error()
            .into();
        }
    }

    let module_url = specifier.as_ref().map(LitStr::value).unwrap_or_else(|| {
        format!(
            "{}:{}",
            prefix.as_ref().expect("prefix checked").value(),
            name.value()
        )
    });

    // Per-export installer fragments. Two shapes:
    //   - capabilities = false → ctx.builtin_method(...)
    //   - capabilities = true  → ctx.method(name, len, HostedNativeCall::dynamic(closure))
    let export_installs = exports.iter().map(|ex| {
        let js_name = &ex.js_name;
        let length = ex.length;
        let call = &ex.call;
        if capabilities {
            quote! {
                {
                    let caps = __lodge_caps.clone();
                    let closure: ::std::sync::Arc<
                        ::otter_runtime::RuntimeNativeFn,
                    > = ::std::sync::Arc::new(
                        move |ctx, args, _captures| #call(ctx, args, &caps),
                    );
                    ctx.method(
                        #js_name,
                        #length,
                        ::otter_runtime::HostedNativeCall::dynamic(closure),
                    )?;
                }
            }
        } else {
            quote! {
                ctx.builtin_method(#js_name, #length, #call)?;
            }
        }
    });

    let prologue = if capabilities {
        quote! {
            let __lodge_caps = ctx.capabilities().clone();
        }
    } else {
        quote! {}
    };

    let install_doc = format!(
        "Install the `{module_url}` hosted module on the supplied \
         [`HostedModuleCtx`]. Generated by `lodge!`."
    );
    let static_doc = format!(
        "Static [`HostedModule`] row for `{module_url}`, ready to drop into a \
         `HOSTED_MODULES` array. Generated by `lodge!`."
    );

    let expanded = quote! {
        #[doc = #install_doc]
        pub fn #install_ident(
            ctx: &mut ::otter_runtime::HostedModuleCtx<'_>,
        ) -> ::core::result::Result<(), ::std::string::String> {
            #prologue
            #(#export_installs)*
            ::core::result::Result::Ok(())
        }

        #[doc = #static_doc]
        pub static #static_ident: ::otter_runtime::HostedModule = ::otter_runtime::HostedModule::new(
            #module_url,
            ::otter_runtime::HostedModuleInstall::new(#install_ident),
        );
    };

    expanded.into()
}
