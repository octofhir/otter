//! `node:url` / `url` hosted module.
//!
//! The CommonJS surface combines a dedicated legacy `Url` implementation with
//! the runtime's WHATWG URL globals. Named ESM imports for the file-URL helpers
//! use small native adapters so Vite-style bootstrap code does not depend on
//! CommonJS interop.
//!
//! # Contents
//! - [`url_cjs_value`] installs legacy and WHATWG Node URL helpers.
//! - [`install_url_module`] exposes the file-URL helpers to ESM.
//! - Native file-path conversion helpers use scoped handles for all results.
//!
//! # Invariants
//! - File URL conversion is pure string/path processing and opens no host
//!   resource, so it requires no filesystem capability.
//! - Every multi-allocation native result is built inside a handle scope.
//! - The shim is parsed once per CommonJS module instantiation, not per call.
//!
//! # See also
//! - Node.js `lib/url.js` and `lib/internal/url.js`.

use std::path::{Path, PathBuf};

use otter_runtime::CapabilitySet;
use otter_vm::{Local, NativeCtx, NativeError, NativeScope, Value};

const SHIM: &str = concat!(include_str!("url.js"), "\n", include_str!("url_legacy.js"));

/// CommonJS export containing WHATWG constructors, legacy helpers, and file
/// URL conversion functions.
pub fn url_cjs_value<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    _caps: &CapabilitySet,
) -> Result<Local<'scope>, String> {
    otter_runtime::run_builtin_cjs_shim(scope, "node:url", SHIM, &[])
}

/// Named ESM surface used by ecosystem loaders.
pub fn install_url_module(ctx: &mut otter_runtime::HostedModuleCtx<'_>) -> Result<(), String> {
    ctx.builtin_method("pathToFileURL", 1, path_to_file_url)?;
    ctx.builtin_method("fileURLToPath", 1, file_url_to_path)?;
    ctx.builtin_method("domainToASCII", 1, domain_to_ascii)?;
    ctx.builtin_method("domainToUnicode", 1, domain_to_unicode)?;
    Ok(())
}

fn path_to_file_url(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let input = required_string(ctx, args, 0, "pathToFileURL", "path")?;
    let href = path_to_file_url_href(&input);
    let constructor = ctx
        .global_value("URL")
        .ok_or_else(|| NativeError::TypeError {
            name: "pathToFileURL",
            reason: "URL constructor is not installed".to_string(),
        })?;
    ctx.scope(|mut scope| {
        let constructor = scope.value(constructor);
        let href = scope.string(&href)?;
        let result = scope.construct(constructor, &[href])?;
        Ok(scope.finish(result))
    })
}

fn file_url_to_path(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let input = file_url_input(ctx, args)?;
    let path = file_url_to_path_string(&input)?;
    ctx.scope(|mut scope| {
        let value = scope.string(&path)?;
        Ok(scope.finish(value))
    })
}

fn domain_to_ascii(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let input = required_string(ctx, args, 0, "domainToASCII", "domain")?;
    let output = idna::domain_to_ascii(&input).unwrap_or_default();
    ctx.scope(|mut scope| {
        let value = scope.string(&output)?;
        Ok(scope.finish(value))
    })
}

fn domain_to_unicode(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let input = required_string(ctx, args, 0, "domainToUnicode", "domain")?;
    let output = idna::domain_to_unicode(&input).0;
    ctx.scope(|mut scope| {
        let value = scope.string(&output)?;
        Ok(scope.finish(value))
    })
}

fn file_url_input(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<String, NativeError> {
    let Some(input) = args.first().copied() else {
        return Err(crate::invalid_arg_type(
            "The \"path\" argument must be of type string or URL. Received undefined",
        ));
    };
    let string_input = input
        .as_string(ctx.heap())
        .map(|string| string.to_lossy_string(ctx.heap()));
    if string_input.is_none() && input.as_object().is_none() {
        return Err(crate::invalid_arg_type(format!(
            "The \"path\" argument must be of type string or URL. Received {}",
            input.display_string(ctx.heap())
        )));
    }
    let constructor = ctx
        .global_value("URL")
        .ok_or_else(|| NativeError::TypeError {
            name: "fileURLToPath",
            reason: "URL constructor is not installed".to_string(),
        })?;
    let value = if let Some(input) = string_input {
        ctx.scope(|mut scope| {
            let constructor = scope.value(constructor);
            let input = scope.string(&input)?;
            let result = scope.construct(constructor, &[input])?;
            Ok::<Value, NativeError>(scope.finish(result))
        })?
    } else {
        input
    };
    let brand_constructor = ctx
        .global_value("URL")
        .ok_or_else(|| NativeError::TypeError {
            name: "fileURLToPath",
            reason: "URL constructor is not installed".to_string(),
        })?;
    if !ctx.is_instance_of(value, brand_constructor)? {
        return Err(crate::invalid_arg_type(
            "The \"path\" argument must be of type string or URL. Received an object",
        ));
    }
    let href = ctx.get_value_property(value, "href")?;
    href.as_string(ctx.heap())
        .map(|string| string.to_lossy_string(ctx.heap()))
        .ok_or_else(|| {
            crate::invalid_arg_type(
                "The \"path\" argument must be of type string or URL. Received an object",
            )
        })
}

fn required_string(
    ctx: &NativeCtx<'_>,
    args: &[Value],
    index: usize,
    function: &'static str,
    argument: &str,
) -> Result<String, NativeError> {
    let Some(value) = args.get(index) else {
        return Err(crate::invalid_arg_type(format!(
            "The \"{argument}\" argument must be of type string. Received undefined"
        )));
    };
    let Some(value) = value.as_string(ctx.heap()) else {
        return Err(crate::invalid_arg_type(format!(
            "The \"{argument}\" argument must be of type string. Received {}",
            value.display_string(ctx.heap())
        )));
    };
    let output = value.to_lossy_string(ctx.heap());
    if function == "fileURLToPath" && !output.starts_with("file:") {
        return Err(NativeError::TypeError {
            name: function,
            reason: "The URL must be of scheme file".to_string(),
        });
    }
    Ok(output)
}

fn path_to_file_url_href(input: &str) -> String {
    let path = if Path::new(input).is_absolute() {
        PathBuf::from(input)
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(input)
    };
    let raw = path.to_string_lossy();
    let normalized = if cfg!(windows) {
        raw.replace('\\', "/")
    } else {
        raw.into_owned()
    };
    let encoded = percent_encode_path(&normalized);
    if cfg!(windows) {
        format!("file:///{encoded}")
    } else {
        format!("file://{encoded}")
    }
}

fn file_url_to_path_string(input: &str) -> Result<String, NativeError> {
    let scheme = input
        .get(..5)
        .filter(|scheme| scheme.eq_ignore_ascii_case("file:"));
    let rest = scheme
        .map(|_| &input[5..])
        .and_then(|rest| rest.strip_prefix("//").or(Some(rest)))
        .ok_or_else(|| NativeError::TypeError {
            name: "fileURLToPath",
            reason: "The URL must be of scheme file".to_string(),
        })?;
    let rest = rest.split(['?', '#']).next().unwrap_or(rest);
    let path = if let Some(after_host) = rest.strip_prefix("localhost/") {
        format!("/{after_host}")
    } else if rest.starts_with('/') {
        rest.to_string()
    } else if rest.is_empty() {
        "/".to_string()
    } else {
        return Err(NativeError::TypeError {
            name: "fileURLToPath",
            reason: "File URL host must be empty or localhost".to_string(),
        });
    };
    percent_decode_path(&path)
}

fn percent_encode_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'_' | b'.' | b'~' | b':') {
            out.push(char::from(byte));
        } else {
            use std::fmt::Write as _;
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

fn percent_decode_path(path: &str) -> Result<String, NativeError> {
    let bytes = path.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'%' {
            decoded.push(bytes[index]);
            index += 1;
            continue;
        }
        if index + 2 >= bytes.len() {
            return Err(invalid_file_url());
        }
        let hi = hex(bytes[index + 1]).ok_or_else(invalid_file_url)?;
        let lo = hex(bytes[index + 2]).ok_or_else(invalid_file_url)?;
        let byte = (hi << 4) | lo;
        if matches!(byte, b'/' | b'\\') {
            return Err(invalid_file_url());
        }
        decoded.push(byte);
        index += 3;
    }
    String::from_utf8(decoded).map_err(|_| invalid_file_url())
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn invalid_file_url() -> NativeError {
    NativeError::TypeError {
        name: "fileURLToPath",
        reason: "File URL path contains invalid percent-encoding".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::{file_url_to_path_string, path_to_file_url_href};

    #[test]
    fn file_url_round_trip_preserves_spaces_and_unicode() {
        let href = path_to_file_url_href("some file-☃.txt");
        assert!(href.contains("some%20file-%E2%98%83.txt"));
        let path = file_url_to_path_string(&href).unwrap();
        assert!(path.ends_with("some file-☃.txt"));
        assert_eq!(
            file_url_to_path_string("FILE:///tmp/upper.txt").unwrap(),
            "/tmp/upper.txt"
        );
    }
}
