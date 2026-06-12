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

/// CommonJS export: the default (posix) `path` namespace with `.posix` and
/// `.win32` sub-namespaces (each cross-linked), matching Node's layout.
pub fn path_cjs_value(ctx: &mut NativeCtx<'_>, _caps: &CapabilitySet) -> Result<Value, String> {
    let mut scope = ModuleScope::new(ctx);

    let posix = scope.object()?;
    for (name, len, f) in PATH_METHODS {
        scope.set_method(posix, name, *len, *f)?;
    }
    scope.set_string(posix, "sep", "/")?;
    scope.set_string(posix, "delimiter", ":")?;

    let win32 = scope.object()?;
    for (name, len, f) in WIN32_METHODS {
        scope.set_method(win32, name, *len, *f)?;
    }
    scope.set_string(win32, "sep", "\\")?;
    scope.set_string(win32, "delimiter", ";")?;

    // Cross-links: each flavor exposes both, like Node.
    scope.set(posix, "posix", posix);
    scope.set(posix, "win32", win32);
    scope.set(win32, "posix", posix);
    scope.set(win32, "win32", win32);

    // Default export is the posix implementation.
    Ok(scope.finish(posix))
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

const WIN32_METHODS: &[Method] = &[
    ("basename", 2, win32_basename),
    ("dirname", 1, win32_dirname),
    ("extname", 1, win32_extname),
    ("isAbsolute", 1, win32_is_absolute),
    ("join", 1, win32_join),
    ("normalize", 1, win32_normalize),
    ("resolve", 1, win32_resolve),
    ("relative", 2, win32_relative),
    ("parse", 1, win32_parse),
    ("format", 1, win32_format),
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

// ---- win32 algorithms (both `/` and `\` are separators) ----

fn win32_is_sep(c: char) -> bool {
    c == '/' || c == '\\'
}

/// Split a Windows path into `(root, rest)`. `root` is the drive/UNC/leading-sep
/// prefix; when it ends in `\` the path is absolute. Drive-relative (`C:foo`)
/// returns root `C:` (no trailing sep).
fn win32_split_root(p: &str) -> (String, &str) {
    let chars: Vec<char> = p.chars().collect();
    if chars.len() >= 2 && chars[1] == ':' && chars[0].is_ascii_alphabetic() {
        let drive = format!("{}:", chars[0]);
        if chars.len() >= 3 && win32_is_sep(chars[2]) {
            return (format!("{drive}\\"), &p[3..]);
        }
        return (drive, &p[2..]);
    }
    if chars.first().is_some_and(|c| win32_is_sep(*c)) {
        return ("\\".to_string(), &p[1..]);
    }
    (String::new(), p)
}

fn win32_is_absolute_str(p: &str) -> bool {
    let (root, _) = win32_split_root(p);
    root.ends_with('\\')
}

fn win32_basename(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let path = arg_string(args, 0, "path.win32.basename", ctx.heap())?;
    let ext = opt_arg_string(args, 1, ctx);
    let base = path
        .rsplit(win32_is_sep)
        .find(|s| !s.is_empty())
        .unwrap_or("");
    // strip a drive-relative prefix like `C:` if no separators were present
    let base = base
        .strip_prefix(|c: char| c.is_ascii_alphabetic())
        .and_then(|r| r.strip_prefix(':'))
        .filter(|_| !path.contains(win32_is_sep))
        .unwrap_or(base);
    let out = match ext.as_deref() {
        Some(ext) if !ext.is_empty() && base.len() > ext.len() && base.ends_with(ext) => {
            &base[..base.len() - ext.len()]
        }
        Some(ext) if base == ext => "",
        _ => base,
    };
    string_value(ctx, out)
}

fn win32_dirname(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let path = arg_string(args, 0, "path.win32.dirname", ctx.heap())?;
    if path.is_empty() {
        return string_value(ctx, ".");
    }
    let (root, rest) = win32_split_root(&path);
    let rest_trimmed = rest.trim_end_matches(win32_is_sep);
    let last = rest_trimmed.rfind(win32_is_sep);
    let out = match last {
        Some(i) => format!("{root}{}", &rest_trimmed[..i]),
        None => {
            if root.is_empty() {
                ".".to_string()
            } else if root.ends_with('\\') {
                root
            } else {
                // drive-relative `C:foo` -> `C:`
                root
            }
        }
    };
    let out = if out.is_empty() { ".".to_string() } else { out };
    string_value(ctx, &out)
}

fn win32_extname(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let path = arg_string(args, 0, "path.win32.extname", ctx.heap())?;
    let base = path
        .rsplit(win32_is_sep)
        .find(|s| !s.is_empty())
        .unwrap_or("");
    let out = match base.rfind('.') {
        Some(i) if i > 0 => &base[i..],
        _ => "",
    };
    string_value(ctx, out)
}

fn win32_is_absolute(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let path = arg_string(args, 0, "path.win32.isAbsolute", ctx.heap())?;
    Ok(Value::boolean(win32_is_absolute_str(&path)))
}

fn win32_normalize_str(p: &str) -> String {
    if p.is_empty() {
        return ".".to_string();
    }
    let (root, rest) = win32_split_root(p);
    let is_abs = root.ends_with('\\');
    let trailing = rest.chars().next_back().is_some_and(win32_is_sep);
    let mut parts: Vec<&str> = Vec::new();
    for seg in rest.split(win32_is_sep) {
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
    let body = parts.join("\\");
    let mut out = root;
    if !out.is_empty() && !out.ends_with('\\') && !body.is_empty() {
        out.push('\\');
    }
    out.push_str(&body);
    if out.is_empty() {
        return ".".to_string();
    }
    if trailing && !out.ends_with('\\') {
        out.push('\\');
    }
    out
}

fn win32_normalize(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let path = arg_string(args, 0, "path.win32.normalize", ctx.heap())?;
    string_value(ctx, &win32_normalize_str(&path))
}

fn win32_join(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let parts: Vec<String> = collect_strings(args, ctx)
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect();
    if parts.is_empty() {
        return string_value(ctx, ".");
    }
    string_value(ctx, &win32_normalize_str(&parts.join("\\")))
}

fn win32_resolve(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let parts = collect_strings(args, ctx);
    let mut resolved = String::new();
    let mut is_abs = false;
    for part in parts.iter().rev() {
        if part.is_empty() {
            continue;
        }
        resolved = if resolved.is_empty() {
            part.clone()
        } else {
            format!("{part}\\{resolved}")
        };
        if win32_is_absolute_str(part) {
            is_abs = true;
            break;
        }
    }
    if !is_abs {
        let base = cwd().replace('/', "\\");
        resolved = if resolved.is_empty() {
            base
        } else {
            format!("{base}\\{resolved}")
        };
    }
    let normalized = win32_normalize_str(&resolved);
    let trimmed = normalized.trim_end_matches('\\');
    let out = if trimmed.is_empty() {
        "\\".to_string()
    } else if trimmed.ends_with(':') {
        format!("{trimmed}\\")
    } else {
        trimmed.to_string()
    };
    string_value(ctx, &out)
}

fn win32_relative(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let from = arg_string(args, 0, "path.win32.relative", ctx.heap())?;
    let to = arg_string(args, 1, "path.win32.relative", ctx.heap())?;
    let from = win32_normalize_str(&from).replace('/', "\\");
    let to = win32_normalize_str(&to).replace('/', "\\");
    if from.eq_ignore_ascii_case(&to) {
        return string_value(ctx, "");
    }
    let from_parts: Vec<&str> = from.split('\\').filter(|s| !s.is_empty()).collect();
    let to_parts: Vec<&str> = to.split('\\').filter(|s| !s.is_empty()).collect();
    let common = from_parts
        .iter()
        .zip(to_parts.iter())
        .take_while(|(a, b)| a.eq_ignore_ascii_case(b))
        .count();
    let mut out: Vec<&str> = Vec::new();
    out.extend(std::iter::repeat_n("..", from_parts.len() - common));
    out.extend_from_slice(&to_parts[common..]);
    string_value(ctx, &out.join("\\"))
}

fn win32_parse(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let path = arg_string(args, 0, "path.win32.parse", ctx.heap())?;
    let (root, _) = win32_split_root(&path);
    let base = path
        .rsplit(win32_is_sep)
        .find(|s| !s.is_empty())
        .unwrap_or("")
        .to_string();
    let ext = match base.rfind('.') {
        Some(i) if i > 0 => base[i..].to_string(),
        _ => String::new(),
    };
    let name = base[..base.len() - ext.len()].to_string();
    let rest_trimmed = path.trim_end_matches(win32_is_sep);
    let dir = match rest_trimmed.rfind(win32_is_sep) {
        Some(0) => "\\".to_string(),
        Some(i) => rest_trimmed[..i].to_string(),
        None => root.trim_end_matches('\\').to_string(),
    };

    let mut scope = ModuleScope::new(ctx);
    let obj = scope.object().map_err(string_err)?;
    scope
        .set_string(obj, "root", root.trim_end_matches('\\'))
        .map_err(string_err)?;
    scope.set_string(obj, "dir", &dir).map_err(string_err)?;
    scope.set_string(obj, "base", &base).map_err(string_err)?;
    scope.set_string(obj, "ext", &ext).map_err(string_err)?;
    scope.set_string(obj, "name", &name).map_err(string_err)?;
    Ok(scope.finish(obj))
}

fn win32_format(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let obj = args
        .first()
        .and_then(|v| v.as_object())
        .ok_or_else(|| crate::type_error("path.win32.format", "argument must be an object"))?;
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
    } else if dir.ends_with('\\') || dir.ends_with('/') {
        format!("{dir}{base}")
    } else {
        format!("{dir}\\{base}")
    };
    string_value(ctx, &out)
}
