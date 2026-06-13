//! `node:path` / `path` hosted module.
//!
//! Pure string algorithms — no I/O — so the whole module is built with
//! [`ModuleScope`]. `resolve`/`relative` consult the process cwd via
//! `std::env::current_dir`.
//!
//! # Contents
//! - `normalize_string`: shared `.`/`..` collapsing core (Node's `normalizeString`).
//! - posix algorithms (`posix_*`) and win32 algorithms (`win32_*`), each a
//!   faithful port of Node v24 `lib/path.js`.
//! - native method wrappers + the CJS/ESM install surface.
//!
//! # Invariants
//! - posix separator `/`, `delimiter` `:`; win32 separator `\`, `delimiter` `;`.
//! - Algorithms operate on `Vec<char>` to mirror Node's UTF-16 `charCodeAt`/
//!   `slice` semantics (BMP-exact) and slice the *original* string rather than
//!   reconstructing it, so original separators are preserved like Node.
//!
//! # See also
//! - Node v24 `lib/path.js` (the reference implementation this mirrors).

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

// ---- shared core ----

fn is_path_sep(c: char) -> bool {
    c == '/' || c == '\\'
}

fn is_posix_sep(c: char) -> bool {
    c == '/'
}

fn is_win_device_root(c: char) -> bool {
    c.is_ascii_alphabetic()
}

/// Faithful port of Node's `normalizeString`: resolves `.`/`..` segments.
/// Operates on a char slice and returns the collapsed body (no leading root).
fn normalize_string(
    path: &[char],
    allow_above_root: bool,
    sep: char,
    is_sep: fn(char) -> bool,
) -> String {
    let mut res: Vec<char> = Vec::new();
    let mut last_segment_length: isize = 0;
    let mut last_slash: isize = -1;
    let mut dots: isize = 0;
    let mut code: char = '\0';
    let len = path.len();
    for i in 0..=len {
        if i < len {
            code = path[i];
        } else if is_sep(code) {
            break;
        } else {
            code = '/';
        }
        let ii = i as isize;
        if is_sep(code) {
            if last_slash == ii - 1 || dots == 1 {
                // NOOP
            } else if dots == 2 {
                let needs = res.len() < 2
                    || last_segment_length != 2
                    || res[res.len() - 1] != '.'
                    || res[res.len() - 2] != '.';
                if needs {
                    if res.len() > 2 {
                        match res.iter().rposition(|&c| c == sep) {
                            None => {
                                res.clear();
                                last_segment_length = 0;
                            }
                            Some(idx) => {
                                res.truncate(idx);
                                let new_last = res
                                    .iter()
                                    .rposition(|&c| c == sep)
                                    .map(|p| p as isize)
                                    .unwrap_or(-1);
                                last_segment_length = res.len() as isize - 1 - new_last;
                            }
                        }
                        last_slash = ii;
                        dots = 0;
                        continue;
                    } else if !res.is_empty() {
                        res.clear();
                        last_segment_length = 0;
                        last_slash = ii;
                        dots = 0;
                        continue;
                    }
                }
                if allow_above_root {
                    if !res.is_empty() {
                        res.push(sep);
                    }
                    res.push('.');
                    res.push('.');
                    last_segment_length = 2;
                }
            } else {
                if !res.is_empty() {
                    res.push(sep);
                }
                res.extend_from_slice(&path[(last_slash + 1) as usize..i]);
                last_segment_length = ii - last_slash - 1;
            }
            last_slash = ii;
            dots = 0;
        } else if code == '.' && dots != -1 {
            dots += 1;
        } else {
            dots = -1;
        }
    }
    res.into_iter().collect()
}

fn cwd() -> String {
    std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "/".to_string())
}

/// `formatExt`: prepend a dot to a non-empty extension that lacks one.
fn format_ext(ext: &str) -> String {
    if ext.is_empty() {
        String::new()
    } else if ext.starts_with('.') {
        ext.to_string()
    } else {
        format!(".{ext}")
    }
}

// ---- posix algorithms ----

fn posix_resolve_str(args: &[String]) -> String {
    let mut resolved = String::new();
    let mut resolved_absolute = false;
    let mut i = args.len() as isize - 1;
    while i >= 0 && !resolved_absolute {
        let path = &args[i as usize];
        if !path.is_empty() {
            resolved = format!("{path}/{resolved}");
            resolved_absolute = path.starts_with('/');
        }
        i -= 1;
    }
    if !resolved_absolute {
        let c = cwd();
        resolved_absolute = c.starts_with('/');
        resolved = format!("{c}/{resolved}");
    }
    let chars: Vec<char> = resolved.chars().collect();
    let normalized = normalize_string(&chars, !resolved_absolute, '/', is_posix_sep);
    if resolved_absolute {
        format!("/{normalized}")
    } else if !normalized.is_empty() {
        normalized
    } else {
        ".".to_string()
    }
}

fn posix_normalize_str(p: &str) -> String {
    if p.is_empty() {
        return ".".to_string();
    }
    let chars: Vec<char> = p.chars().collect();
    let is_absolute = chars[0] == '/';
    let trailing = chars[chars.len() - 1] == '/';
    let normalized = normalize_string(&chars, !is_absolute, '/', is_posix_sep);
    if normalized.is_empty() {
        if is_absolute {
            return "/".to_string();
        }
        return if trailing { "./" } else { "." }.to_string();
    }
    let mut out = normalized;
    if trailing {
        out.push('/');
    }
    if is_absolute { format!("/{out}") } else { out }
}

fn posix_join_str(args: &[String]) -> String {
    if args.is_empty() {
        return ".".to_string();
    }
    let parts: Vec<&str> = args
        .iter()
        .map(String::as_str)
        .filter(|s| !s.is_empty())
        .collect();
    if parts.is_empty() {
        return ".".to_string();
    }
    posix_normalize_str(&parts.join("/"))
}

fn posix_relative_str(from_in: &str, to_in: &str) -> String {
    if from_in == to_in {
        return String::new();
    }
    let from = posix_resolve_str(std::slice::from_ref(&from_in.to_string()));
    let to = posix_resolve_str(std::slice::from_ref(&to_in.to_string()));
    if from == to {
        return String::new();
    }
    let from_c: Vec<char> = from.chars().collect();
    let to_c: Vec<char> = to.chars().collect();
    let from_start = 1isize;
    let from_end = from_c.len() as isize;
    let from_len = from_end - from_start;
    let to_start = 1isize;
    let to_len = to_c.len() as isize - to_start;
    let length = from_len.min(to_len);
    let mut last_common_sep: isize = -1;
    let mut i = 0isize;
    while i < length {
        let fc = from_c[(from_start + i) as usize];
        if fc != to_c[(to_start + i) as usize] {
            break;
        } else if fc == '/' {
            last_common_sep = i;
        }
        i += 1;
    }
    if i == length {
        if to_len > length {
            if to_c[(to_start + i) as usize] == '/' {
                return to_c[(to_start + i + 1) as usize..].iter().collect();
            }
            if i == 0 {
                return to_c[(to_start + i) as usize..].iter().collect();
            }
        } else if from_len > length {
            if from_c[(from_start + i) as usize] == '/' {
                last_common_sep = i;
            } else if i == 0 {
                last_common_sep = 0;
            }
        }
    }
    let mut out = String::new();
    let mut k = from_start + last_common_sep + 1;
    while k <= from_end {
        if k == from_end || from_c[k as usize] == '/' {
            out.push_str(if out.is_empty() { ".." } else { "/.." });
        }
        k += 1;
    }
    let tail: String = to_c[(to_start + last_common_sep) as usize..]
        .iter()
        .collect();
    format!("{out}{tail}")
}

/// Faithful port of Node posix `dirname` (preserves a leading `//`).
fn posix_dirname_str(p: &str) -> String {
    let chars: Vec<char> = p.chars().collect();
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

fn posix_basename_str(p: &str, suffix: Option<&str>) -> String {
    let path: Vec<char> = p.chars().collect();
    let mut start = 0usize;
    let mut end: isize = -1;
    let mut matched_slash = true;
    if let Some(suf) = suffix {
        let suffix_c: Vec<char> = suf.chars().collect();
        if !suffix_c.is_empty() && suffix_c.len() <= path.len() {
            if suf == p {
                return String::new();
            }
            let mut ext_idx: isize = suffix_c.len() as isize - 1;
            let mut first_non_slash_end: isize = -1;
            let mut i = path.len() as isize - 1;
            while i >= 0 {
                let code = path[i as usize];
                if code == '/' {
                    if !matched_slash {
                        start = (i + 1) as usize;
                        break;
                    }
                } else {
                    if first_non_slash_end == -1 {
                        matched_slash = false;
                        first_non_slash_end = i + 1;
                    }
                    if ext_idx >= 0 {
                        if code == suffix_c[ext_idx as usize] {
                            ext_idx -= 1;
                            if ext_idx == -1 {
                                end = i;
                            }
                        } else {
                            ext_idx = -1;
                            end = first_non_slash_end;
                        }
                    }
                }
                i -= 1;
            }
            if start as isize == end {
                end = first_non_slash_end;
            } else if end == -1 {
                end = path.len() as isize;
            }
            return path[start..end as usize].iter().collect();
        }
    }
    let mut i = path.len() as isize - 1;
    while i >= 0 {
        if path[i as usize] == '/' {
            if !matched_slash {
                start = (i + 1) as usize;
                break;
            }
        } else if end == -1 {
            matched_slash = false;
            end = i + 1;
        }
        i -= 1;
    }
    if end == -1 {
        return String::new();
    }
    path[start..end as usize].iter().collect()
}

fn posix_extname_str(p: &str) -> String {
    let path: Vec<char> = p.chars().collect();
    let mut start_dot: isize = -1;
    let mut start_part = 0isize;
    let mut end: isize = -1;
    let mut matched_slash = true;
    let mut pre_dot_state = 0i32;
    let mut i = path.len() as isize - 1;
    while i >= 0 {
        let c = path[i as usize];
        if c == '/' {
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
    path[start_dot as usize..end as usize].iter().collect()
}

/// Returns `(root, dir, base, ext, name)`.
fn posix_parse_parts(p: &str) -> (String, String, String, String, String) {
    let path: Vec<char> = p.chars().collect();
    let mut root = String::new();
    let mut dir = String::new();
    let mut base = String::new();
    let mut ext = String::new();
    let mut name = String::new();
    if path.is_empty() {
        return (root, dir, base, ext, name);
    }
    let is_absolute = path[0] == '/';
    let start: isize = if is_absolute {
        root = "/".to_string();
        1
    } else {
        0
    };
    let mut start_dot: isize = -1;
    let mut start_part = 0isize;
    let mut end: isize = -1;
    let mut matched_slash = true;
    let mut pre_dot_state = 0i32;
    let mut i = path.len() as isize - 1;
    while i >= start {
        let code = path[i as usize];
        if code == '/' {
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
        if code == '.' {
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
    if end != -1 {
        let st = if start_part == 0 && is_absolute {
            1
        } else {
            start_part
        };
        if start_dot == -1
            || pre_dot_state == 0
            || (pre_dot_state == 1 && start_dot == end - 1 && start_dot == start_part + 1)
        {
            base = path[st as usize..end as usize].iter().collect();
            name = base.clone();
        } else {
            name = path[st as usize..start_dot as usize].iter().collect();
            base = path[st as usize..end as usize].iter().collect();
            ext = path[start_dot as usize..end as usize].iter().collect();
        }
    }
    if start_part > 0 {
        dir = path[..(start_part - 1) as usize].iter().collect();
    } else if is_absolute {
        dir = "/".to_string();
    }
    (root, dir, base, ext, name)
}

// ---- win32 algorithms ----

fn win32_is_absolute_str(p: &str) -> bool {
    let path: Vec<char> = p.chars().collect();
    let len = path.len();
    if len == 0 {
        return false;
    }
    let code = path[0];
    is_path_sep(code)
        || (len > 2 && is_win_device_root(code) && path[1] == ':' && is_path_sep(path[2]))
}

fn win32_resolve_str(args: &[String]) -> String {
    let mut resolved_device = String::new();
    let mut resolved_tail = String::new();
    let mut resolved_absolute = false;
    let mut idx: isize = args.len() as isize - 1;
    while idx >= -1 {
        let path_owned: String;
        if idx >= 0 {
            let p = &args[idx as usize];
            if p.is_empty() {
                idx -= 1;
                continue;
            }
            path_owned = p.clone();
        } else if resolved_device.is_empty() {
            path_owned = cwd();
        } else {
            // Windows tracks per-drive cwd via `process.env['=C:']`; we don't on
            // non-Windows hosts, so fall back to the process cwd and apply the
            // same drive-mismatch guard Node uses.
            let mut p = cwd();
            let pch: Vec<char> = p.chars().collect();
            let first2: String = pch.iter().take(2).collect();
            if first2.to_lowercase() != resolved_device.to_lowercase() && pch.get(2) == Some(&'\\')
            {
                p = format!("{resolved_device}\\");
            }
            path_owned = p;
        }
        let path: Vec<char> = path_owned.chars().collect();
        let len = path.len();
        let mut root_end = 0usize;
        let mut device = String::new();
        let mut is_absolute = false;
        let code = path.first().copied().unwrap_or('\0');
        if len == 1 {
            if is_path_sep(code) {
                root_end = 1;
                is_absolute = true;
            }
        } else if is_path_sep(code) {
            is_absolute = true;
            if is_path_sep(path[1]) {
                let mut j = 2usize;
                let mut last = j;
                while j < len && !is_path_sep(path[j]) {
                    j += 1;
                }
                if j < len && j != last {
                    let first_part: String = path[last..j].iter().collect();
                    last = j;
                    while j < len && is_path_sep(path[j]) {
                        j += 1;
                    }
                    if j < len && j != last {
                        last = j;
                        while j < len && !is_path_sep(path[j]) {
                            j += 1;
                        }
                        if j == len || j != last {
                            if first_part != "." && first_part != "?" {
                                let share: String = path[last..j].iter().collect();
                                device = format!("\\\\{first_part}\\{share}");
                                root_end = j;
                            } else {
                                device = format!("\\\\{first_part}");
                                root_end = 4;
                            }
                        }
                    }
                }
            } else {
                root_end = 1;
            }
        } else if is_win_device_root(code) && path.get(1) == Some(&':') {
            device = path[0..2].iter().collect();
            root_end = 2;
            if len > 2 && is_path_sep(path[2]) {
                is_absolute = true;
                root_end = 3;
            }
        }

        if !device.is_empty() {
            if !resolved_device.is_empty() {
                if device.to_lowercase() != resolved_device.to_lowercase() {
                    idx -= 1;
                    continue;
                }
            } else {
                resolved_device = device.clone();
            }
        }

        if resolved_absolute {
            if !resolved_device.is_empty() {
                break;
            }
        } else {
            let tail_slice: String = path[root_end..].iter().collect();
            resolved_tail = format!("{tail_slice}\\{resolved_tail}");
            resolved_absolute = is_absolute;
            if is_absolute && !resolved_device.is_empty() {
                break;
            }
        }
        idx -= 1;
    }
    let tail_chars: Vec<char> = resolved_tail.chars().collect();
    let normalized_tail = normalize_string(&tail_chars, !resolved_absolute, '\\', is_path_sep);
    if resolved_absolute {
        format!("{resolved_device}\\{normalized_tail}")
    } else {
        let r = format!("{resolved_device}{normalized_tail}");
        if r.is_empty() { ".".to_string() } else { r }
    }
}

fn win32_normalize_str(p: &str) -> String {
    let path: Vec<char> = p.chars().collect();
    let len = path.len();
    if len == 0 {
        return ".".to_string();
    }
    let mut root_end = 0usize;
    let mut device: Option<String> = None;
    let mut is_absolute = false;
    let code = path[0];
    if len == 1 {
        return if is_posix_sep(code) {
            "\\".to_string()
        } else {
            p.to_string()
        };
    }
    if is_path_sep(code) {
        is_absolute = true;
        if is_path_sep(path[1]) {
            let mut j = 2usize;
            let mut last = j;
            while j < len && !is_path_sep(path[j]) {
                j += 1;
            }
            if j < len && j != last {
                let first_part: String = path[last..j].iter().collect();
                last = j;
                while j < len && is_path_sep(path[j]) {
                    j += 1;
                }
                if j < len && j != last {
                    last = j;
                    while j < len && !is_path_sep(path[j]) {
                        j += 1;
                    }
                    if j == len || j != last {
                        if first_part == "." || first_part == "?" {
                            device = Some(format!("\\\\{first_part}"));
                            root_end = 4;
                        } else if j == len {
                            let share: String = path[last..].iter().collect();
                            return format!("\\\\{first_part}\\{share}\\");
                        } else {
                            let share: String = path[last..j].iter().collect();
                            device = Some(format!("\\\\{first_part}\\{share}"));
                            root_end = j;
                        }
                    }
                }
            }
        } else {
            root_end = 1;
        }
    } else if is_win_device_root(code) && path.get(1) == Some(&':') {
        device = Some(path[0..2].iter().collect());
        root_end = 2;
        if len > 2 && is_path_sep(path[2]) {
            is_absolute = true;
            root_end = 3;
        }
    }

    let mut tail = if root_end < len {
        normalize_string(&path[root_end..], !is_absolute, '\\', is_path_sep)
    } else {
        String::new()
    };
    if tail.is_empty() && !is_absolute {
        tail = ".".to_string();
    }
    if !tail.is_empty() && is_path_sep(path[len - 1]) {
        tail.push('\\');
    }
    if !is_absolute && device.is_none() && p.contains(':') {
        // CVE-2024-36139: a relative path must not normalize into something
        // Windows would read as device-absolute.
        let tail_c: Vec<char> = tail.chars().collect();
        if tail_c.len() >= 2 && is_win_device_root(tail_c[0]) && tail_c[1] == ':' {
            return format!(".\\{tail}");
        }
        let mut search = 0usize;
        while let Some(rel) = p[search..].find(':') {
            let index = search + rel;
            let next = path.get(index + 1).copied();
            if index == len - 1 || next.is_some_and(is_path_sep) {
                return format!(".\\{tail}");
            }
            search = index + 1;
            if search >= p.len() {
                break;
            }
        }
    }
    match device {
        None => {
            if is_absolute {
                format!("\\{tail}")
            } else {
                tail
            }
        }
        Some(dev) => {
            if is_absolute {
                format!("{dev}\\{tail}")
            } else {
                format!("{dev}{tail}")
            }
        }
    }
}

fn win32_join_str(args: &[String]) -> String {
    if args.is_empty() {
        return ".".to_string();
    }
    let mut joined: Option<String> = None;
    let mut first_part = String::new();
    for arg in args {
        if !arg.is_empty() {
            match &mut joined {
                None => {
                    joined = Some(arg.clone());
                    first_part = arg.clone();
                }
                Some(j) => {
                    j.push('\\');
                    j.push_str(arg);
                }
            }
        }
    }
    let Some(mut joined) = joined else {
        return ".".to_string();
    };
    let fp: Vec<char> = first_part.chars().collect();
    let mut needs_replace = true;
    let mut slash_count = 0usize;
    if !fp.is_empty() && is_path_sep(fp[0]) {
        slash_count += 1;
        let first_len = fp.len();
        if first_len > 1 && is_path_sep(fp[1]) {
            slash_count += 1;
            if first_len > 2 {
                if is_path_sep(fp[2]) {
                    slash_count += 1;
                } else {
                    needs_replace = false;
                }
            }
        }
    }
    if needs_replace {
        let jc: Vec<char> = joined.chars().collect();
        while slash_count < jc.len() && is_path_sep(jc[slash_count]) {
            slash_count += 1;
        }
        if slash_count >= 2 {
            let rest: String = jc[slash_count..].iter().collect();
            joined = format!("\\{rest}");
        }
    }
    // CVE-2025-27210: skip normalization when a reserved device name is present,
    // so `..` traversal past a device segment (`CON:..\..`) is not collapsed.
    let jc: Vec<char> = joined.chars().collect();
    let mut parts: Vec<String> = Vec::new();
    let mut part = String::new();
    let mut i = 0usize;
    while i < jc.len() {
        if jc[i] == '\\' {
            if !part.is_empty() {
                parts.push(std::mem::take(&mut part));
            }
            while i + 1 < jc.len() && jc[i + 1] == '\\' {
                i += 1;
            }
        } else {
            part.push(jc[i]);
        }
        i += 1;
    }
    if !part.is_empty() {
        parts.push(part);
    }
    if parts.iter().any(|p| is_windows_reserved_name(p)) {
        return joined.replace('/', "\\");
    }
    win32_normalize_str(&joined)
}

/// `isWindowsReservedName`: the part before a `:` is a reserved device name
/// (`CON`, `PRN`, `COM1`…, `LPT1`…, including the `¹²³` superscript variants).
fn is_windows_reserved_name(p: &str) -> bool {
    let Some(colon) = p.find(':') else {
        return false;
    };
    let device = p[..colon].to_uppercase();
    const NAMES: &[&str] = &[
        "CON",
        "PRN",
        "AUX",
        "NUL",
        "COM1",
        "COM2",
        "COM3",
        "COM4",
        "COM5",
        "COM6",
        "COM7",
        "COM8",
        "COM9",
        "LPT1",
        "LPT2",
        "LPT3",
        "LPT4",
        "LPT5",
        "LPT6",
        "LPT7",
        "LPT8",
        "LPT9",
        "COM\u{b9}",
        "COM\u{b2}",
        "COM\u{b3}",
        "LPT\u{b9}",
        "LPT\u{b2}",
        "LPT\u{b3}",
    ];
    NAMES.contains(&device.as_str())
}

fn win32_relative_str(from_in: &str, to_in: &str) -> String {
    if from_in == to_in {
        return String::new();
    }
    let from_orig = win32_resolve_str(std::slice::from_ref(&from_in.to_string()));
    let to_orig = win32_resolve_str(std::slice::from_ref(&to_in.to_string()));
    if from_orig == to_orig {
        return String::new();
    }
    let from = from_orig.to_lowercase();
    let to = to_orig.to_lowercase();
    if from == to {
        return String::new();
    }

    // When lowercasing changes length (e.g. `İ` -> `i̇`), index alignment with
    // the original strings is lost, so fall back to segment-wise comparison.
    if from_orig.chars().count() != from.chars().count()
        || to_orig.chars().count() != to.chars().count()
    {
        let mut from_split: Vec<&str> = from_orig.split('\\').collect();
        let mut to_split: Vec<&str> = to_orig.split('\\').collect();
        if from_split.last() == Some(&"") {
            from_split.pop();
        }
        if to_split.last() == Some(&"") {
            to_split.pop();
        }
        let from_len = from_split.len();
        let to_len = to_split.len();
        let length = from_len.min(to_len);
        let mut i = 0usize;
        while i < length {
            if from_split[i].to_lowercase() != to_split[i].to_lowercase() {
                break;
            }
            i += 1;
        }
        if i == 0 {
            return to_orig;
        } else if i == length {
            if to_len > length {
                return to_split[i..].join("\\");
            }
            if from_len > length {
                return "..\\".repeat(from_len - 1 - i) + "..";
            }
            return String::new();
        }
        return "..\\".repeat(from_len - i) + &to_split[i..].join("\\");
    }

    let from_c: Vec<char> = from.chars().collect();
    let to_c: Vec<char> = to.chars().collect();
    let to_orig_c: Vec<char> = to_orig.chars().collect();
    let mut from_start = 0usize;
    while from_start < from_c.len() && from_c[from_start] == '\\' {
        from_start += 1;
    }
    let mut from_end = from_c.len();
    while from_end as isize - 1 > from_start as isize && from_c[from_end - 1] == '\\' {
        from_end -= 1;
    }
    let from_len = from_end - from_start;
    let mut to_start = 0usize;
    while to_start < to_c.len() && to_c[to_start] == '\\' {
        to_start += 1;
    }
    let mut to_end = to_c.len();
    while to_end as isize - 1 > to_start as isize && to_c[to_end - 1] == '\\' {
        to_end -= 1;
    }
    let to_len = to_end - to_start;
    let length = from_len.min(to_len);
    let mut last_common_sep: isize = -1;
    let mut i = 0usize;
    while i < length {
        let fc = from_c[from_start + i];
        if fc != to_c[to_start + i] {
            break;
        } else if fc == '\\' {
            last_common_sep = i as isize;
        }
        i += 1;
    }
    if i != length {
        if last_common_sep == -1 {
            return to_orig;
        }
    } else {
        if to_len > length {
            if to_c[to_start + i] == '\\' {
                return to_orig_c[to_start + i + 1..].iter().collect();
            }
            if i == 2 {
                return to_orig_c[to_start + i..].iter().collect();
            }
        }
        if from_len > length {
            if from_c[from_start + i] == '\\' {
                last_common_sep = i as isize;
            } else if i == 2 {
                last_common_sep = 3;
            }
        }
        if last_common_sep == -1 {
            last_common_sep = 0;
        }
    }
    let mut out = String::new();
    let mut k = from_start as isize + last_common_sep + 1;
    while k <= from_end as isize {
        if k == from_end as isize || from_c[k as usize] == '\\' {
            out.push_str(if out.is_empty() { ".." } else { "\\.." });
        }
        k += 1;
    }
    let to_start2 = to_start + last_common_sep as usize;
    if !out.is_empty() {
        let tail: String = to_orig_c[to_start2..to_end].iter().collect();
        return format!("{out}{tail}");
    }
    let mut ts = to_start2;
    if to_orig_c.get(ts) == Some(&'\\') {
        ts += 1;
    }
    to_orig_c[ts..to_end].iter().collect()
}

fn win32_dirname_str(p: &str) -> String {
    let path: Vec<char> = p.chars().collect();
    let len = path.len();
    if len == 0 {
        return ".".to_string();
    }
    let mut root_end: isize = -1;
    let mut offset = 0usize;
    let code = path[0];
    if len == 1 {
        return if is_path_sep(code) {
            p.to_string()
        } else {
            ".".to_string()
        };
    }
    if is_path_sep(code) {
        root_end = 1;
        offset = 1;
        if is_path_sep(path[1]) {
            let mut j = 2usize;
            let mut last = j;
            while j < len && !is_path_sep(path[j]) {
                j += 1;
            }
            if j < len && j != last {
                last = j;
                while j < len && is_path_sep(path[j]) {
                    j += 1;
                }
                if j < len && j != last {
                    last = j;
                    while j < len && !is_path_sep(path[j]) {
                        j += 1;
                    }
                    if j == len {
                        return p.to_string();
                    }
                    if j != last {
                        root_end = (j + 1) as isize;
                        offset = j + 1;
                    }
                }
            }
        }
    } else if is_win_device_root(code) && path.get(1) == Some(&':') {
        root_end = if len > 2 && is_path_sep(path[2]) {
            3
        } else {
            2
        };
        offset = root_end as usize;
    }
    let mut end: isize = -1;
    let mut matched_slash = true;
    let mut i = len as isize - 1;
    while i >= offset as isize {
        if is_path_sep(path[i as usize]) {
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
        if root_end == -1 {
            return ".".to_string();
        }
        end = root_end;
    }
    path[..end as usize].iter().collect()
}

fn win32_basename_str(p: &str, suffix: Option<&str>) -> String {
    let path: Vec<char> = p.chars().collect();
    let mut start = 0usize;
    let mut end: isize = -1;
    let mut matched_slash = true;
    if path.len() >= 2 && is_win_device_root(path[0]) && path[1] == ':' {
        start = 2;
    }
    if let Some(suf) = suffix {
        let suffix_c: Vec<char> = suf.chars().collect();
        if !suffix_c.is_empty() && suffix_c.len() <= path.len() {
            if suf == p {
                return String::new();
            }
            let mut ext_idx: isize = suffix_c.len() as isize - 1;
            let mut first_non_slash_end: isize = -1;
            let mut i = path.len() as isize - 1;
            while i >= start as isize {
                let code = path[i as usize];
                if is_path_sep(code) {
                    if !matched_slash {
                        start = (i + 1) as usize;
                        break;
                    }
                } else {
                    if first_non_slash_end == -1 {
                        matched_slash = false;
                        first_non_slash_end = i + 1;
                    }
                    if ext_idx >= 0 {
                        if code == suffix_c[ext_idx as usize] {
                            ext_idx -= 1;
                            if ext_idx == -1 {
                                end = i;
                            }
                        } else {
                            ext_idx = -1;
                            end = first_non_slash_end;
                        }
                    }
                }
                i -= 1;
            }
            if start as isize == end {
                end = first_non_slash_end;
            } else if end == -1 {
                end = path.len() as isize;
            }
            return path[start..end as usize].iter().collect();
        }
    }
    let mut i = path.len() as isize - 1;
    while i >= start as isize {
        if is_path_sep(path[i as usize]) {
            if !matched_slash {
                start = (i + 1) as usize;
                break;
            }
        } else if end == -1 {
            matched_slash = false;
            end = i + 1;
        }
        i -= 1;
    }
    if end == -1 {
        return String::new();
    }
    path[start..end as usize].iter().collect()
}

fn win32_extname_str(p: &str) -> String {
    let path: Vec<char> = p.chars().collect();
    let mut start = 0usize;
    let mut start_dot: isize = -1;
    let mut start_part = 0isize;
    let mut end: isize = -1;
    let mut matched_slash = true;
    let mut pre_dot_state = 0i32;
    if path.len() >= 2 && path[1] == ':' && is_win_device_root(path[0]) {
        start = 2;
        start_part = 2;
    }
    let mut i = path.len() as isize - 1;
    while i >= start as isize {
        let code = path[i as usize];
        if is_path_sep(code) {
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
        if code == '.' {
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
    path[start_dot as usize..end as usize].iter().collect()
}

/// Returns `(root, dir, base, ext, name)`.
fn win32_parse_parts(p: &str) -> (String, String, String, String, String) {
    let path: Vec<char> = p.chars().collect();
    let mut root = String::new();
    let mut dir = String::new();
    let mut base = String::new();
    let mut ext = String::new();
    let mut name = String::new();
    if path.is_empty() {
        return (root, dir, base, ext, name);
    }
    let len = path.len();
    let mut root_end = 0usize;
    let code = path[0];
    if len == 1 {
        if is_path_sep(code) {
            root = p.to_string();
            dir = p.to_string();
            return (root, dir, base, ext, name);
        }
        base = p.to_string();
        name = p.to_string();
        return (root, dir, base, ext, name);
    }
    if is_path_sep(code) {
        root_end = 1;
        if is_path_sep(path[1]) {
            let mut j = 2usize;
            let mut last = j;
            while j < len && !is_path_sep(path[j]) {
                j += 1;
            }
            if j < len && j != last {
                last = j;
                while j < len && is_path_sep(path[j]) {
                    j += 1;
                }
                if j < len && j != last {
                    last = j;
                    while j < len && !is_path_sep(path[j]) {
                        j += 1;
                    }
                    if j == len {
                        root_end = j;
                    } else if j != last {
                        root_end = j + 1;
                    }
                }
            }
        }
    } else if is_win_device_root(code) && path.get(1) == Some(&':') {
        if len <= 2 {
            root = p.to_string();
            dir = p.to_string();
            return (root, dir, base, ext, name);
        }
        root_end = 2;
        if is_path_sep(path[2]) {
            if len == 3 {
                root = p.to_string();
                dir = p.to_string();
                return (root, dir, base, ext, name);
            }
            root_end = 3;
        }
    }
    if root_end > 0 {
        root = path[..root_end].iter().collect();
    }
    let mut start_dot: isize = -1;
    let mut start_part = root_end as isize;
    let mut end: isize = -1;
    let mut matched_slash = true;
    let mut pre_dot_state = 0i32;
    let mut i = len as isize - 1;
    while i >= root_end as isize {
        let c = path[i as usize];
        if is_path_sep(c) {
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
    if end != -1 {
        if start_dot == -1
            || pre_dot_state == 0
            || (pre_dot_state == 1 && start_dot == end - 1 && start_dot == start_part + 1)
        {
            base = path[start_part as usize..end as usize].iter().collect();
            name = base.clone();
        } else {
            name = path[start_part as usize..start_dot as usize]
                .iter()
                .collect();
            base = path[start_part as usize..end as usize].iter().collect();
            ext = path[start_dot as usize..end as usize].iter().collect();
        }
    }
    if start_part > 0 && start_part != root_end as isize {
        dir = path[..(start_part - 1) as usize].iter().collect();
    } else {
        dir = root.clone();
    }
    (root, dir, base, ext, name)
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
    string_value(ctx, &posix_basename_str(&path, ext.as_deref()))
}

fn path_dirname(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let path = require_str(args, 0, ctx)?;
    string_value(ctx, &posix_dirname_str(&path))
}

fn path_extname(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let path = require_str(args, 0, ctx)?;
    string_value(ctx, &posix_extname_str(&path))
}

fn path_is_absolute(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let path = require_str(args, 0, ctx)?;
    Ok(Value::boolean(!path.is_empty() && path.starts_with('/')))
}

fn path_join(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let parts = collect_strings(args, ctx)?;
    string_value(ctx, &posix_join_str(&parts))
}

fn path_normalize(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let path = require_str(args, 0, ctx)?;
    string_value(ctx, &posix_normalize_str(&path))
}

fn path_resolve(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let parts = collect_strings(args, ctx)?;
    string_value(ctx, &posix_resolve_str(&parts))
}

fn path_relative(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let from = require_str(args, 0, ctx)?;
    let to = require_str(args, 1, ctx)?;
    string_value(ctx, &posix_relative_str(&from, &to))
}

fn build_parse_object(
    ctx: &mut NativeCtx<'_>,
    parts: (String, String, String, String, String),
) -> Result<Value, NativeError> {
    let (root, dir, base, ext, name) = parts;
    let mut scope = ModuleScope::new(ctx);
    let obj = scope.ordinary_object().map_err(string_err)?;
    scope.set_string(obj, "root", &root).map_err(string_err)?;
    scope.set_string(obj, "dir", &dir).map_err(string_err)?;
    scope.set_string(obj, "base", &base).map_err(string_err)?;
    scope.set_string(obj, "ext", &ext).map_err(string_err)?;
    scope.set_string(obj, "name", &name).map_err(string_err)?;
    Ok(scope.finish(obj))
}

fn path_parse(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let path = require_str(args, 0, ctx)?;
    build_parse_object(ctx, posix_parse_parts(&path))
}

/// Shared `_format` body (`dir === root ? dir+base : dir+sep+base`).
fn format_path(ctx: &mut NativeCtx<'_>, args: &[Value], sep: char) -> Result<Value, NativeError> {
    let arg = args.first().copied().unwrap_or_else(Value::undefined);
    let Some(obj) = arg.as_object() else {
        let suffix = arg_type_helper(&arg, ctx.heap());
        return Err(invalid_arg_type(format!(
            "The \"pathObject\" argument must be of type object.{suffix}"
        )));
    };
    let heap = ctx.heap();
    let read = |key: &str| -> Option<String> {
        otter_vm::object::get(obj, heap, key)
            .filter(|v| !v.is_undefined() && !v.is_null())
            .map(|v| v.display_string(heap))
    };
    let truthy = |o: &Option<String>| o.as_deref().is_some_and(|s| !s.is_empty());

    let dir_prop = read("dir");
    let root_prop = read("root");
    let dir = if truthy(&dir_prop) {
        dir_prop.unwrap()
    } else {
        root_prop.clone().unwrap_or_default()
    };
    let base_prop = read("base");
    let base = if truthy(&base_prop) {
        base_prop.unwrap()
    } else {
        let name = read("name").filter(|s| !s.is_empty()).unwrap_or_default();
        let ext = read("ext").unwrap_or_default();
        format!("{name}{}", format_ext(&ext))
    };
    if dir.is_empty() {
        return string_value(ctx, &base);
    }
    // `dir === pathObject.root`: the *effective* dir equals the raw root value.
    let dir_is_root = root_prop.as_deref() == Some(dir.as_str());
    let out = if dir_is_root {
        format!("{dir}{base}")
    } else {
        format!("{dir}{sep}{base}")
    };
    string_value(ctx, &out)
}

fn path_format(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    format_path(ctx, args, '/')
}

fn string_err(message: String) -> NativeError {
    crate::type_error("path", message)
}

/// Node's `invalidArgTypeHelper` suffix for a rejected value (` Received ...`).
fn arg_type_helper(v: &Value, heap: &otter_gc::GcHeap) -> String {
    if v.is_undefined() {
        " Received undefined".to_string()
    } else if v.is_null() {
        " Received null".to_string()
    } else if v.is_string() {
        format!(" Received type string ('{}')", v.display_string(heap))
    } else if v.is_boolean() {
        format!(" Received type boolean ({})", v.display_string(heap))
    } else if v.is_number() {
        format!(" Received type number ({})", v.display_string(heap))
    } else {
        format!(" Received {}", v.display_string(heap))
    }
}

/// posix `toNamespacedPath` is the identity (non-strings pass through too).
fn path_to_namespaced(_ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    Ok(args.first().copied().unwrap_or_else(Value::undefined))
}

fn win32_basename(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let path = require_str(args, 0, ctx)?;
    let ext = opt_arg_string(args, 1, ctx)?;
    string_value(ctx, &win32_basename_str(&path, ext.as_deref()))
}

fn win32_dirname(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let path = require_str(args, 0, ctx)?;
    string_value(ctx, &win32_dirname_str(&path))
}

fn win32_extname(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let path = require_str(args, 0, ctx)?;
    string_value(ctx, &win32_extname_str(&path))
}

fn win32_is_absolute(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let path = require_str(args, 0, ctx)?;
    Ok(Value::boolean(win32_is_absolute_str(&path)))
}

fn win32_normalize(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let path = require_str(args, 0, ctx)?;
    string_value(ctx, &win32_normalize_str(&path))
}

fn win32_join(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let parts = collect_strings(args, ctx)?;
    string_value(ctx, &win32_join_str(&parts))
}

fn win32_resolve(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let parts = collect_strings(args, ctx)?;
    string_value(ctx, &win32_resolve_str(&parts))
}

fn win32_relative(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let from = require_str(args, 0, ctx)?;
    let to = require_str(args, 1, ctx)?;
    string_value(ctx, &win32_relative_str(&from, &to))
}

fn win32_parse(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let path = require_str(args, 0, ctx)?;
    build_parse_object(ctx, win32_parse_parts(&path))
}

fn win32_format(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    format_path(ctx, args, '\\')
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
    string_value(ctx, &win32_to_namespaced_str(&path))
}

fn win32_to_namespaced_str(p: &str) -> String {
    let resolved = win32_resolve_str(std::slice::from_ref(&p.to_string()));
    let rc: Vec<char> = resolved.chars().collect();
    if rc.len() <= 2 {
        return p.to_string();
    }
    if rc[0] == '\\' {
        if rc[1] == '\\' {
            let code = rc[2];
            if code != '?' && code != '.' {
                let rest: String = rc[2..].iter().collect();
                return format!("\\\\?\\UNC\\{rest}");
            }
        }
    } else if is_win_device_root(rc[0]) && rc[1] == ':' && rc[2] == '\\' {
        return format!("\\\\?\\{resolved}");
    }
    resolved
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
