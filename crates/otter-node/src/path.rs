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

use crate::{invalid_arg_type, string_value};

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
    ("toNamespacedPath", 1, path_to_namespaced),
    ("matchesGlob", 2, path_matches_glob),
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
    ("toNamespacedPath", 1, win32_to_namespaced),
    ("matchesGlob", 2, win32_matches_glob),
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
    let body = parts.join("/");
    if body.is_empty() {
        // Node: empty result keeps a relative trailing slash as "./".
        return if is_abs {
            "/".into()
        } else if trailing {
            "./".into()
        } else {
            ".".into()
        };
    }
    let mut out = if is_abs { format!("/{body}") } else { body };
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
        // Node: `ext === path` => "". Otherwise strip the suffix only when the
        // basename is strictly longer (a whole-component match is NOT stripped).
        if path == ext {
            return String::new();
        }
        if !ext.is_empty() && base.len() > ext.len() && base.ends_with(ext) {
            return base[..base.len() - ext.len()].to_string();
        }
    }
    base.to_string()
}

/// Faithful port of Node's posix `dirname` (preserves a leading `//`).
fn dirname(path: &str) -> String {
    let chars: Vec<char> = path.chars().collect();
    if chars.is_empty() {
        return ".".to_string();
    }
    let has_root = chars[0] == '/';
    let mut end: isize = -1;
    let mut matched_slash = true;
    let mut i = chars.len() as isize - 1;
    while i >= 1 {
        if chars[i as usize] == '/' {
            if !matched_slash {
                end = i;
                break;
            }
        } else {
            matched_slash = false;
        }
        i -= 1;
    }
    if end == -1 {
        return if has_root { "/" } else { "." }.to_string();
    }
    if has_root && end == 1 {
        return "//".to_string();
    }
    chars[..end as usize].iter().collect()
}

/// Node's `extname` algorithm (faithful port of `lib/path.js`), parameterised by
/// the separator predicate so posix and win32 share it. Handles leading-dot /
/// all-dots basenames (`..` -> "", `..file` -> ".file", `file.` -> ".").
fn extname_with(path: &str, is_sep: fn(char) -> bool) -> String {
    let chars: Vec<char> = path.chars().collect();
    let mut start_dot: isize = -1;
    let mut start_part: isize = 0;
    let mut end: isize = -1;
    let mut matched_slash = true;
    let mut pre_dot_state: i32 = 0;
    let mut i = chars.len() as isize - 1;
    while i >= 0 {
        let c = chars[i as usize];
        if is_sep(c) {
            if !matched_slash {
                start_part = i + 1;
                break;
            }
            i -= 1;
            continue;
        }
        if end == -1 {
            matched_slash = false;
            end = i + 1;
        }
        if c == '.' {
            if start_dot == -1 {
                start_dot = i;
            } else if pre_dot_state != 1 {
                pre_dot_state = 1;
            }
        } else if start_dot != -1 {
            pre_dot_state = -1;
        }
        i -= 1;
    }
    if start_dot == -1
        || end == -1
        || pre_dot_state == 0
        || (pre_dot_state == 1 && start_dot == end - 1 && start_dot == start_part + 1)
    {
        return String::new();
    }
    chars[start_dot as usize..end as usize].iter().collect()
}

fn extname(path: &str) -> String {
    extname_with(path, |c| c == '/')
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

/// A required string path argument. Throws Node's `ERR_INVALID_ARG_TYPE`
/// (a `TypeError` carrying `.code`) when the value is missing or not a string.
fn require_str(args: &[Value], i: usize, ctx: &mut NativeCtx<'_>) -> Result<String, NativeError> {
    match args.get(i) {
        Some(v) if v.is_string() => Ok(otter_runtime::runtime_arg_to_string(args, i, ctx.heap())),
        _ => Err(invalid_arg_type(
            "The \"path\" argument must be of type string.",
        )),
    }
}

/// An optional string argument (e.g. `basename`'s suffix): `undefined`/absent is
/// `None`; any other non-string throws `ERR_INVALID_ARG_TYPE`.
fn opt_arg_string(
    args: &[Value],
    i: usize,
    ctx: &mut NativeCtx<'_>,
) -> Result<Option<String>, NativeError> {
    match args.get(i) {
        None => Ok(None),
        Some(v) if v.is_undefined() => Ok(None),
        Some(v) if v.is_string() => Ok(Some(otter_runtime::runtime_arg_to_string(
            args,
            i,
            ctx.heap(),
        ))),
        _ => Err(invalid_arg_type(
            "The \"suffix\" argument must be of type string.",
        )),
    }
}

/// Collect variadic path segments, each of which must be a string
/// (`join`/`resolve`). An empty arg list is allowed; a non-string arg throws.
fn collect_strings(args: &[Value], ctx: &mut NativeCtx<'_>) -> Result<Vec<String>, NativeError> {
    let mut out = Vec::with_capacity(args.len());
    for (i, v) in args.iter().enumerate() {
        if !v.is_string() {
            return Err(invalid_arg_type(
                "The \"path\" argument must be of type string.",
            ));
        }
        out.push(otter_runtime::runtime_arg_to_string(args, i, ctx.heap()));
    }
    Ok(out)
}

fn path_basename(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let path = require_str(args, 0, ctx)?;
    let ext = opt_arg_string(args, 1, ctx)?;
    string_value(ctx, &basename(&path, ext.as_deref()))
}

fn path_dirname(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let path = require_str(args, 0, ctx)?;
    string_value(ctx, &dirname(&path))
}

fn path_extname(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let path = require_str(args, 0, ctx)?;
    string_value(ctx, &extname(&path))
}

fn path_is_absolute(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let path = require_str(args, 0, ctx)?;
    Ok(Value::boolean(path.starts_with('/')))
}

fn path_join(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let parts = collect_strings(args, ctx)?;
    string_value(ctx, &join(&parts))
}

fn path_normalize(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let path = require_str(args, 0, ctx)?;
    string_value(ctx, &normalize_posix(&path))
}

fn path_resolve(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let parts = collect_strings(args, ctx)?;
    string_value(ctx, &resolve(&parts))
}

fn path_relative(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let from = require_str(args, 0, ctx)?;
    let to = require_str(args, 1, ctx)?;
    string_value(ctx, &relative(&from, &to))
}

fn path_parse(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let path = require_str(args, 0, ctx)?;
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

/// posix `toNamespacedPath` is the identity (non-strings pass through too).
fn path_to_namespaced(_ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    Ok(args.first().copied().unwrap_or_else(Value::undefined))
}

/// win32 `toNamespacedPath`: prefix absolute drive/UNC paths with `\\?\`.
fn win32_to_namespaced(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let Some(first) = args.first() else {
        return Ok(Value::undefined());
    };
    if !first.is_string() {
        return Ok(*first);
    }
    let path = otter_runtime::runtime_arg_to_string(args, 0, ctx.heap());
    if path.is_empty() {
        return Ok(*first);
    }
    let resolved = win32_resolve_str(std::slice::from_ref(&path));
    if resolved.len() <= 2 {
        return string_value(ctx, &path);
    }
    let b = resolved.as_bytes();
    let out = if b[0] == b'\\' && b.get(1) == Some(&b'\\') {
        // UNC: \\server\share -> \\?\UNC\server\share (unless already \\?\).
        if b.get(2) == Some(&b'?') {
            resolved.clone()
        } else {
            format!("\\\\?\\UNC\\{}", &resolved[2..])
        }
    } else if b[0].is_ascii_alphabetic() && b.get(1) == Some(&b':') && b.get(2) == Some(&b'\\') {
        format!("\\\\?\\{resolved}")
    } else {
        resolved.clone()
    };
    string_value(ctx, &out)
}

/// posix `matchesGlob(path, pattern)` — glob matching via `globset` (supports
/// `*`, `**`, `?`, and `[...]` character classes).
fn path_matches_glob(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let path = require_str(args, 0, ctx)?;
    let pattern = require_str(args, 1, ctx)?;
    Ok(Value::boolean(glob_matches(&pattern, &path)))
}

/// win32 `matchesGlob` — both `/` and `\` are separators; normalise to `/`.
fn win32_matches_glob(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let path = require_str(args, 0, ctx)?.replace('\\', "/");
    let pattern = require_str(args, 1, ctx)?.replace('\\', "/");
    Ok(Value::boolean(glob_matches(&pattern, &path)))
}

fn glob_matches(pattern: &str, path: &str) -> bool {
    globset::GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()
        .map(|g| g.compile_matcher().is_match(path))
        .unwrap_or(false)
}

// ---- win32 algorithms (both `/` and `\` are separators) ----

fn win32_is_sep(c: char) -> bool {
    c == '/' || c == '\\'
}

/// Split a Windows path into `(device, is_absolute, rest)`. `device` is the
/// drive (`C:`) or UNC (`\\server\share`) prefix *without* the root separator;
/// `is_absolute` is whether a root separator follows the device; `rest` is the
/// remainder after the device + optional root sep. Separator bytes are ASCII so
/// byte slicing at their positions is safe.
fn win32_split_root(p: &str) -> (String, bool, &str) {
    let b = p.as_bytes();
    let is_sep_b = |c: u8| c == b'/' || c == b'\\';
    if b.len() >= 2 && b[1] == b':' && b[0].is_ascii_alphabetic() {
        let device = format!("{}:", b[0] as char);
        if b.len() >= 3 && is_sep_b(b[2]) {
            return (device, true, &p[3..]);
        }
        return (device, false, &p[2..]);
    }
    if b.len() >= 2 && is_sep_b(b[0]) && is_sep_b(b[1]) {
        // UNC: \\server\share
        let mut i = 2;
        while i < b.len() && !is_sep_b(b[i]) {
            i += 1;
        }
        if i > 2 && i < b.len() {
            let server_end = i;
            let mut j = i + 1;
            while j < b.len() && !is_sep_b(b[j]) {
                j += 1;
            }
            if j > i + 1 {
                let device = format!("\\\\{}\\{}", &p[2..server_end], &p[i + 1..j]);
                // Skip the root separator that follows the share, if present, so
                // `rest` never starts with it (UNC paths are always absolute).
                let rest = if j < b.len() { &p[j + 1..] } else { &p[j..] };
                return (device, true, rest);
            }
        }
        // Too many / malformed leading slashes: treat as a plain absolute root.
        return (String::new(), true, &p[1..]);
    }
    if !b.is_empty() && is_sep_b(b[0]) {
        return (String::new(), true, &p[1..]);
    }
    (String::new(), false, p)
}

fn win32_is_absolute_str(p: &str) -> bool {
    win32_split_root(p).1
}

fn win32_basename(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let path = require_str(args, 0, ctx)?;
    let ext = opt_arg_string(args, 1, ctx)?;
    // Strip the drive/UNC root first so `C:\` / `C:` basename to "".
    let (_, _, rest) = win32_split_root(&path);
    let base = rest
        .rsplit(win32_is_sep)
        .find(|s| !s.is_empty())
        .unwrap_or("");
    let out = match ext.as_deref() {
        Some(ext) if path == ext => "",
        Some(ext) if !ext.is_empty() && base.len() > ext.len() && base.ends_with(ext) => {
            &base[..base.len() - ext.len()]
        }
        _ => base,
    };
    string_value(ctx, out)
}

fn win32_dirname(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let path = require_str(args, 0, ctx)?;
    if path.is_empty() {
        return string_value(ctx, ".");
    }
    let (device, is_abs, rest) = win32_split_root(&path);
    let root = if is_abs {
        format!("{device}\\")
    } else {
        device.clone()
    };
    let rest_trimmed = rest.trim_end_matches(win32_is_sep);
    let out = match rest_trimmed.rfind(win32_is_sep) {
        Some(i) => format!("{root}{}", &rest_trimmed[..i]),
        // A single component below the root: dirname is the root (with sep).
        None if !rest_trimmed.is_empty() => {
            if root.is_empty() {
                ".".to_string()
            } else {
                root
            }
        }
        // No component at all (path is just the root): a UNC root is its own
        // dirname (no trailing sep); a drive root keeps its separator.
        None if device.starts_with("\\\\") => device,
        None if !root.is_empty() => root,
        None => ".".to_string(),
    };
    let out = if out.is_empty() { ".".to_string() } else { out };
    string_value(ctx, &out)
}

fn win32_extname(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let path = require_str(args, 0, ctx)?;
    // Strip the drive/UNC root so the extension scan starts at the basename.
    let (_, _, rest) = win32_split_root(&path);
    string_value(ctx, &extname_with(rest, win32_is_sep))
}

fn win32_is_absolute(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let path = require_str(args, 0, ctx)?;
    Ok(Value::boolean(win32_is_absolute_str(&path)))
}

fn win32_normalize_str(p: &str) -> String {
    if p.is_empty() {
        return ".".to_string();
    }
    let (device, is_abs, rest) = win32_split_root(p);
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
    let mut body = parts.join("\\");
    if body.is_empty() && !is_abs {
        body.push('.');
    }
    if !body.is_empty() && trailing && !body.ends_with('\\') {
        body.push('\\');
    }
    let mut out = device;
    if is_abs {
        out.push('\\');
    }
    out.push_str(&body);
    if out.is_empty() {
        out.push('.');
    }
    out
}

fn win32_normalize(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let path = require_str(args, 0, ctx)?;
    string_value(ctx, &win32_normalize_str(&path))
}

fn win32_join(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let parts: Vec<String> = collect_strings(args, ctx)?
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect();
    if parts.is_empty() {
        return string_value(ctx, ".");
    }
    string_value(ctx, &win32_normalize_str(&parts.join("\\")))
}

fn win32_resolve_str(parts: &[String]) -> String {
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
    if trimmed.is_empty() {
        "\\".to_string()
    } else if trimmed.ends_with(':') {
        format!("{trimmed}\\")
    } else {
        trimmed.to_string()
    }
}

fn win32_resolve(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let parts = collect_strings(args, ctx)?;
    let out = win32_resolve_str(&parts);
    string_value(ctx, &out)
}

fn win32_relative(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let from = require_str(args, 0, ctx)?;
    let to = require_str(args, 1, ctx)?;
    let from = win32_normalize_str(&from).replace('/', "\\");
    let to = win32_normalize_str(&to).replace('/', "\\");
    if from.eq_ignore_ascii_case(&to) {
        return string_value(ctx, "");
    }
    // Different drive/UNC roots cannot be relativised — return the target.
    let (from_dev, _, _) = win32_split_root(&from);
    let (to_dev, _, _) = win32_split_root(&to);
    if !from_dev.eq_ignore_ascii_case(&to_dev) {
        return string_value(ctx, &to);
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
    let path = require_str(args, 0, ctx)?;
    let (device, is_abs, rest) = win32_split_root(&path);
    let root = if is_abs {
        format!("{device}\\")
    } else {
        device.clone()
    };
    let base = rest
        .rsplit(win32_is_sep)
        .find(|s| !s.is_empty())
        .unwrap_or("")
        .to_string();
    let ext = extname_with(&base, win32_is_sep);
    let name = base[..base.len() - ext.len()].to_string();
    let rest_trimmed = rest.trim_end_matches(win32_is_sep);
    let dir = match rest_trimmed.rfind(win32_is_sep) {
        Some(i) => format!("{root}{}", &rest_trimmed[..i]),
        None => root.clone(),
    };

    let mut scope = ModuleScope::new(ctx);
    let obj = scope.object().map_err(string_err)?;
    scope.set_string(obj, "root", &root).map_err(string_err)?;
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
