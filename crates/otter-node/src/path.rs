//! `node:path` / `path` hosted module (POSIX semantics).
//!
//! Pure string algorithms — no I/O — so the whole module is built with
//! [`ModuleScope`]. `resolve`/`relative` consult the process cwd via
//! `std::env::current_dir`.
//!
//! # Invariants
//! - POSIX separator `/`; `sep` = "/", `delimiter` = ":".
//! - Algorithms mirror Node's `lib/path.js` posix branch.

use otter_runtime::CapabilitySet;
use otter_runtime::module_scope::ModuleScope;
use otter_vm::{NativeCtx, NativeError, Value};

use crate::{arg_string, string_value};

/// ESM namespace install (methods only; `sep`/`delimiter` are on the CJS value).
pub fn install_path_module(ctx: &mut otter_runtime::HostedModuleCtx<'_>) -> Result<(), String> {
    for (name, len, f) in PATH_METHODS {
        ctx.builtin_method(name, *len, *f)?;
    }
    Ok(())
}

/// CommonJS export: the path namespace object with methods + `sep`/`delimiter`,
/// and a `posix` self-reference.
pub fn path_cjs_value(ctx: &mut NativeCtx<'_>, _caps: &CapabilitySet) -> Result<Value, String> {
    let mut scope = ModuleScope::new(ctx);
    let path = scope.object()?;
    for (name, len, f) in PATH_METHODS {
        scope.set_method(path, name, *len, *f)?;
    }
    scope.set_string(path, "sep", "/")?;
    scope.set_string(path, "delimiter", ":")?;
    // `path.posix` refers back to the posix implementation (this object).
    scope.set(path, "posix", path);
    Ok(scope.finish(path))
}

type Method = (
    &'static str,
    u8,
    fn(&mut NativeCtx<'_>, &[Value]) -> Result<Value, NativeError>,
);

const PATH_METHODS: &[Method] = &[
    ("basename", 2, path_basename),
    ("dirname", 1, path_dirname),
    ("extname", 1, path_extname),
    ("isAbsolute", 1, path_is_absolute),
    ("join", 1, path_join),
    ("normalize", 1, path_normalize),
    ("resolve", 1, path_resolve),
    ("relative", 2, path_relative),
    ("parse", 1, path_parse),
    ("format", 1, path_format),
];

// ---- pure POSIX algorithms ----

/// Normalize `.`/`..`/`//` segments, preserving a leading and trailing slash.
fn normalize_posix(input: &str) -> String {
    if input.is_empty() {
        return ".".to_string();
    }
    let is_abs = input.starts_with('/');
    let trailing = input.len() > 1 && input.ends_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for seg in input.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                if let Some(&last) = parts.last() {
                    if last == ".." {
                        parts.push("..");
                    } else {
                        parts.pop();
                    }
                } else if !is_abs {
                    parts.push("..");
                }
            }
            other => parts.push(other),
        }
    }
    let mut out = parts.join("/");
    if is_abs {
        out = format!("/{out}");
    }
    if out.is_empty() {
        return if is_abs { "/".into() } else { ".".into() };
    }
    if trailing && !out.ends_with('/') {
        out.push('/');
    }
    out
}

/// Last path component (after stripping trailing slashes).
fn basename_of(path: &str) -> &str {
    path.rsplit('/').find(|s| !s.is_empty()).unwrap_or("")
}

fn basename(path: &str, ext: Option<&str>) -> String {
    let base = basename_of(path);
    if let Some(ext) = ext {
        if !ext.is_empty() && base.len() > ext.len() && base.ends_with(ext) {
            return base[..base.len() - ext.len()].to_string();
        }
        if base == ext {
            return String::new();
        }
    }
    base.to_string()
}

fn dirname(path: &str) -> String {
    if path.is_empty() {
        return ".".to_string();
    }
    let has_root = path.starts_with('/');
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return "/".to_string();
    }
    match trimmed.rfind('/') {
        None => {
            if has_root {
                "/".to_string()
            } else {
                ".".to_string()
            }
        }
        Some(0) => "/".to_string(),
        Some(i) => trimmed[..i].to_string(),
    }
}

fn extname(path: &str) -> String {
    let base = basename_of(path);
    match base.rfind('.') {
        Some(i) if i > 0 => base[i..].to_string(),
        _ => String::new(),
    }
}

fn join(parts: &[String]) -> String {
    let joined: Vec<&str> = parts
        .iter()
        .map(String::as_str)
        .filter(|s| !s.is_empty())
        .collect();
    if joined.is_empty() {
        return ".".to_string();
    }
    normalize_posix(&joined.join("/"))
}

fn cwd() -> String {
    std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "/".to_string())
}

fn resolve(parts: &[String]) -> String {
    let mut resolved = String::new();
    let mut is_abs = false;
    for part in parts.iter().rev() {
        if part.is_empty() {
            continue;
        }
        resolved = if resolved.is_empty() {
            part.clone()
        } else {
            format!("{part}/{resolved}")
        };
        if part.starts_with('/') {
            is_abs = true;
            break;
        }
    }
    if !is_abs {
        let base = cwd();
        resolved = if resolved.is_empty() {
            base
        } else {
            format!("{base}/{resolved}")
        };
    }
    let normalized = normalize_posix(&resolved);
    // resolve() yields an absolute path with no trailing slash (except root).
    let trimmed = normalized.trim_end_matches('/');
    if trimmed.is_empty() {
        "/".to_string()
    } else {
        trimmed.to_string()
    }
}

fn relative(from: &str, to: &str) -> String {
    let from = resolve(std::slice::from_ref(&from.to_string()));
    let to = resolve(std::slice::from_ref(&to.to_string()));
    if from == to {
        return String::new();
    }
    let from_parts: Vec<&str> = from.split('/').filter(|s| !s.is_empty()).collect();
    let to_parts: Vec<&str> = to.split('/').filter(|s| !s.is_empty()).collect();
    let common = from_parts
        .iter()
        .zip(to_parts.iter())
        .take_while(|(a, b)| a == b)
        .count();
    let mut out: Vec<&str> = Vec::new();
    out.extend(std::iter::repeat_n("..", from_parts.len() - common));
    out.extend_from_slice(&to_parts[common..]);
    out.join("/")
}

// ---- native method wrappers ----

fn opt_arg_string(args: &[Value], i: usize, ctx: &mut NativeCtx<'_>) -> Option<String> {
    args.get(i)
        .filter(|v| !v.is_undefined())
        .map(|_| otter_runtime::runtime_arg_to_string(args, i, ctx.heap()))
}

fn collect_strings(args: &[Value], ctx: &mut NativeCtx<'_>) -> Vec<String> {
    (0..args.len())
        .map(|i| otter_runtime::runtime_arg_to_string(args, i, ctx.heap()))
        .collect()
}

fn path_basename(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let path = arg_string(args, 0, "path.basename", ctx.heap())?;
    let ext = opt_arg_string(args, 1, ctx);
    string_value(ctx, &basename(&path, ext.as_deref()))
}

fn path_dirname(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let path = arg_string(args, 0, "path.dirname", ctx.heap())?;
    string_value(ctx, &dirname(&path))
}

fn path_extname(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let path = arg_string(args, 0, "path.extname", ctx.heap())?;
    string_value(ctx, &extname(&path))
}

fn path_is_absolute(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let path = arg_string(args, 0, "path.isAbsolute", ctx.heap())?;
    Ok(Value::boolean(path.starts_with('/')))
}

fn path_join(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let parts = collect_strings(args, ctx);
    string_value(ctx, &join(&parts))
}

fn path_normalize(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let path = arg_string(args, 0, "path.normalize", ctx.heap())?;
    string_value(ctx, &normalize_posix(&path))
}

fn path_resolve(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let parts = collect_strings(args, ctx);
    string_value(ctx, &resolve(&parts))
}

fn path_relative(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let from = arg_string(args, 0, "path.relative", ctx.heap())?;
    let to = arg_string(args, 1, "path.relative", ctx.heap())?;
    string_value(ctx, &relative(&from, &to))
}

fn path_parse(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let path = arg_string(args, 0, "path.parse", ctx.heap())?;
    let root = if path.starts_with('/') { "/" } else { "" };
    let base = basename_of(&path).to_string();
    let ext = extname(&path);
    let name = base[..base.len() - ext.len()].to_string();
    // dir = everything up to the last slash of the trailing-stripped path.
    let trimmed = path.trim_end_matches('/');
    let dir = match trimmed.rfind('/') {
        Some(0) => "/".to_string(),
        Some(i) => trimmed[..i].to_string(),
        None => String::new(),
    };

    let mut scope = ModuleScope::new(ctx);
    let obj = scope.object().map_err(string_err)?;
    scope.set_string(obj, "root", root).map_err(string_err)?;
    scope.set_string(obj, "dir", &dir).map_err(string_err)?;
    scope.set_string(obj, "base", &base).map_err(string_err)?;
    scope.set_string(obj, "ext", &ext).map_err(string_err)?;
    scope.set_string(obj, "name", &name).map_err(string_err)?;
    Ok(scope.finish(obj))
}

fn path_format(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let obj = args
        .first()
        .and_then(|v| v.as_object())
        .ok_or_else(|| crate::type_error("path.format", "argument must be an object"))?;
    let heap = ctx.heap();
    let read = |key: &str| -> Option<String> {
        otter_vm::object::get(obj, heap, key)
            .filter(|v| !v.is_undefined() && !v.is_null())
            .map(|v| v.display_string(heap))
    };
    let dir = read("dir").or_else(|| read("root")).unwrap_or_default();
    let base = read("base").unwrap_or_else(|| {
        format!(
            "{}{}",
            read("name").unwrap_or_default(),
            read("ext").unwrap_or_default()
        )
    });
    let out = if dir.is_empty() {
        base
    } else if dir.ends_with('/') {
        format!("{dir}{base}")
    } else {
        format!("{dir}/{base}")
    };
    string_value(ctx, &out)
}

fn string_err(message: String) -> NativeError {
    crate::type_error("path", message)
}
