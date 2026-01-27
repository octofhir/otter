//! RegExp built-in
//!
//! Provides RegExp constructor and all RegExp.prototype methods:
//! - Static: escape (ES2026)
//! - Properties: source, flags, global, ignoreCase, multiline, dotAll, sticky, unicode, unicodeSets, hasIndices, lastIndex
//! - Methods: test, exec, toString
//! - Symbol methods: match, matchAll, replace, search, split

use dashmap::DashMap;
use otter_vm_core::memory;
use otter_vm_core::string::JsString;
use otter_vm_core::value::Value;
use otter_vm_runtime::{Op, op_native_with_mm as op_native};
use regress::{Flags, Regex, escape};
use std::sync::{Arc, OnceLock};

static REGEX_CACHE: OnceLock<DashMap<(String, String), Arc<Regex>>> = OnceLock::new();

fn get_regex_cache() -> &'static DashMap<(String, String), Arc<Regex>> {
    REGEX_CACHE.get_or_init(DashMap::new)
}

/// Get RegExp ops for extension registration
pub fn ops() -> Vec<Op> {
    vec![
        // Static methods
        op_native("__RegExp_escape", regexp_escape),
        // Property getters
        op_native("__RegExp_source", regexp_source),
        op_native("__RegExp_flags", regexp_flags),
        op_native("__RegExp_global", regexp_global),
        op_native("__RegExp_ignoreCase", regexp_ignore_case),
        op_native("__RegExp_multiline", regexp_multiline),
        op_native("__RegExp_dotAll", regexp_dot_all),
        op_native("__RegExp_sticky", regexp_sticky),
        op_native("__RegExp_unicode", regexp_unicode),
        op_native("__RegExp_unicodeSets", regexp_unicode_sets),
        op_native("__RegExp_hasIndices", regexp_has_indices),
        // Methods
        op_native("__RegExp_test", regexp_test),
        op_native("__RegExp_exec", regexp_exec),
        op_native("__RegExp_toString", regexp_to_string),
        // Symbol methods (used by String.prototype)
        op_native("__RegExp_match", regexp_match),
        op_native("__RegExp_matchAll", regexp_match_all),
        op_native("__RegExp_replace", regexp_replace),
        op_native("__RegExp_search", regexp_search),
        op_native("__RegExp_split", regexp_split),
    ]
}

// =============================================================================
// Helper types and functions
// =============================================================================

/// Parse a JS regex pattern and flags into Rust regex
/// Args: [pattern, flags]
fn parse_regex_args(args: &[Value]) -> Option<(String, String)> {
    let pattern = args.first()?.as_string()?.to_string();
    let flags = args
        .get(1)
        .and_then(|v| v.as_string())
        .map(|s| s.to_string())
        .unwrap_or_default();
    Some((pattern, flags))
}

fn flags_use_unicode(flags: &str) -> bool {
    flags.contains('u') || flags.contains('v')
}

/// Build JS regex from pattern and flags using regress.
fn build_regex(pattern: &str, flags: &str) -> Result<Arc<Regex>, String> {
    let cache = get_regex_cache();
    let key = (pattern.to_string(), flags.to_string());

    if let Some(re) = cache.get(&key) {
        return Ok(re.clone());
    }

    let re = Arc::new(
        Regex::with_flags(pattern, Flags::from(flags))
            .map_err(|e| format!("Invalid regular expression: {}", e))?,
    );

    cache.insert(key, re.clone());
    Ok(re)
}

fn find_first(
    regex: &Regex,
    input: &JsString,
    flags: &str,
    start: usize,
) -> Option<regress::Match> {
    if flags_use_unicode(flags) {
        regex.find_from_utf16(input.as_utf16(), start).next()
    } else {
        regex.find_from_ucs2(input.as_utf16(), start).next()
    }
}

fn find_all(regex: &Regex, input: &JsString, flags: &str) -> Vec<regress::Match> {
    let mut matches = Vec::new();
    let mut start = 0;
    let len = input.len_utf16();

    while start <= len {
        let next = find_first(regex, input, flags, start);
        let Some(mat) = next else { break };
        let end = mat.end();
        let begin = mat.start();
        matches.push(mat);
        if end == begin {
            start = end.saturating_add(1);
        } else {
            start = end;
        }
    }

    matches
}

fn slice_utf16(input: &JsString, range: std::ops::Range<usize>) -> String {
    input
        .substring_utf16(range.start, range.end)
        .as_str()
        .to_string()
}

fn match_to_strings(input: &JsString, mat: &regress::Match) -> Vec<String> {
    let mut out = Vec::with_capacity(mat.captures.len() + 1);
    for idx in 0..=mat.captures.len() {
        if let Some(range) = mat.group(idx) {
            out.push(slice_utf16(input, range));
        } else {
            out.push(String::new());
        }
    }
    out
}

// =============================================================================
// Static methods
// =============================================================================

/// RegExp.escape(string) - ES2026
/// Escapes all regex special characters in the string
fn regexp_escape(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, String> {
    let s = args
        .first()
        .and_then(|v| v.as_string())
        .ok_or("RegExp.escape requires a string")?;

    let escaped = escape(s.as_str());
    Ok(Value::string(JsString::intern(&escaped)))
}

// =============================================================================
// Property getters
// =============================================================================

/// RegExp.prototype.source - returns the pattern string
fn regexp_source(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, String> {
    let pattern = args
        .first()
        .and_then(|v| v.as_string())
        .ok_or("Invalid RegExp")?;
    Ok(Value::string(JsString::intern(pattern.as_str())))
}

/// RegExp.prototype.flags - returns the flags string
fn regexp_flags(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, String> {
    let flags = args
        .get(1)
        .and_then(|v| v.as_string())
        .map(|s| s.to_string())
        .unwrap_or_default();
    Ok(Value::string(JsString::intern(&flags)))
}

/// RegExp.prototype.global - returns true if 'g' flag is set
fn regexp_global(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, String> {
    let flags = args
        .get(1)
        .and_then(|v| v.as_string())
        .map(|s| s.to_string())
        .unwrap_or_default();
    Ok(Value::boolean(flags.contains('g')))
}

/// RegExp.prototype.ignoreCase - returns true if 'i' flag is set
fn regexp_ignore_case(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, String> {
    let flags = args
        .get(1)
        .and_then(|v| v.as_string())
        .map(|s| s.to_string())
        .unwrap_or_default();
    Ok(Value::boolean(flags.contains('i')))
}

/// RegExp.prototype.multiline - returns true if 'm' flag is set
fn regexp_multiline(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, String> {
    let flags = args
        .get(1)
        .and_then(|v| v.as_string())
        .map(|s| s.to_string())
        .unwrap_or_default();
    Ok(Value::boolean(flags.contains('m')))
}

/// RegExp.prototype.dotAll - returns true if 's' flag is set (ES2018)
fn regexp_dot_all(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, String> {
    let flags = args
        .get(1)
        .and_then(|v| v.as_string())
        .map(|s| s.to_string())
        .unwrap_or_default();
    Ok(Value::boolean(flags.contains('s')))
}

/// RegExp.prototype.sticky - returns true if 'y' flag is set
fn regexp_sticky(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, String> {
    let flags = args
        .get(1)
        .and_then(|v| v.as_string())
        .map(|s| s.to_string())
        .unwrap_or_default();
    Ok(Value::boolean(flags.contains('y')))
}

/// RegExp.prototype.unicode - returns true if 'u' flag is set
fn regexp_unicode(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, String> {
    let flags = args
        .get(1)
        .and_then(|v| v.as_string())
        .map(|s| s.to_string())
        .unwrap_or_default();
    Ok(Value::boolean(flags.contains('u')))
}

/// RegExp.prototype.unicodeSets - returns true if 'v' flag is set (ES2024)
fn regexp_unicode_sets(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, String> {
    let flags = args
        .get(1)
        .and_then(|v| v.as_string())
        .map(|s| s.to_string())
        .unwrap_or_default();
    Ok(Value::boolean(flags.contains('v')))
}

/// RegExp.prototype.hasIndices - returns true if 'd' flag is set (ES2022)
fn regexp_has_indices(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, String> {
    let flags = args
        .get(1)
        .and_then(|v| v.as_string())
        .map(|s| s.to_string())
        .unwrap_or_default();
    Ok(Value::boolean(flags.contains('d')))
}

// =============================================================================
// Methods
// =============================================================================

/// RegExp.prototype.test(string) - returns true if pattern matches
fn regexp_test(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, String> {
    let (pattern, flags) = parse_regex_args(args).ok_or("Invalid RegExp")?;
    let input = args
        .get(2)
        .and_then(|v| v.as_string())
        .ok_or("test requires a string argument")?;

    let regex = build_regex(&pattern, &flags)?;
    let found = find_first(&regex, &input, &flags, 0).is_some();
    Ok(Value::boolean(found))
}

/// RegExp.prototype.exec(string) - returns match array or null
/// Returns: [fullMatch, ...captureGroups] with index and input properties
fn regexp_exec(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, String> {
    let (pattern, flags) = parse_regex_args(args).ok_or("Invalid RegExp")?;
    let input = args
        .get(2)
        .and_then(|v| v.as_string())
        .ok_or("exec requires a string argument")?;

    let regex = build_regex(&pattern, &flags)?;

    match find_first(&regex, &input, &flags, 0) {
        Some(mat) => {
            let matches = match_to_strings(&input, &mat);
            let result = serde_json::json!({
                "matches": matches,
                "index": mat.start(),
                "input": input.as_str()
            });
            Ok(Value::string(JsString::intern(&result.to_string())))
        }
        None => Ok(Value::null()),
    }
}

/// RegExp.prototype.toString() - returns "/pattern/flags"
fn regexp_to_string(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, String> {
    let pattern = args
        .first()
        .and_then(|v| v.as_string())
        .map(|s| s.to_string())
        .unwrap_or_default();
    let flags = args
        .get(1)
        .and_then(|v| v.as_string())
        .map(|s| s.to_string())
        .unwrap_or_default();

    Ok(Value::string(JsString::intern(&format!(
        "/{}/{}",
        pattern, flags
    ))))
}

// =============================================================================
// Symbol methods (String.prototype integration)
// =============================================================================

/// RegExp.prototype[Symbol.match](string) - used by String.prototype.match
fn regexp_match(args: &[Value], mm: Arc<memory::MemoryManager>) -> Result<Value, String> {
    let (pattern, flags) = parse_regex_args(args).ok_or("Invalid RegExp")?;
    let input = args
        .get(2)
        .and_then(|v| v.as_string())
        .ok_or("match requires a string argument")?;

    let regex = build_regex(&pattern, &flags)?;
    let is_global = flags.contains('g');

    if is_global {
        // Global: return all matches
        let matches: Vec<String> = find_all(&regex, &input, &flags)
            .into_iter()
            .map(|m| slice_utf16(&input, m.range()))
            .collect();

        if matches.is_empty() {
            Ok(Value::null())
        } else {
            let result = serde_json::to_string(&matches).unwrap_or_else(|_| "[]".to_string());
            Ok(Value::string(JsString::intern(&result)))
        }
    } else {
        // Non-global: same as exec
        regexp_exec(args, mm)
    }
}

/// RegExp.prototype[Symbol.matchAll](string) - ES2020
/// Returns iterator of all matches with capture groups
fn regexp_match_all(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, String> {
    let (pattern, flags) = parse_regex_args(args).ok_or("Invalid RegExp")?;
    let input = args
        .get(2)
        .and_then(|v| v.as_string())
        .ok_or("matchAll requires a string argument")?;

    // matchAll requires global flag
    if !flags.contains('g') {
        return Err("matchAll must be called with a global RegExp".to_string());
    }

    let regex = build_regex(&pattern, &flags)?;

    let mut all_matches: Vec<serde_json::Value> = Vec::new();
    for mat in find_all(&regex, &input, &flags) {
        let matches = match_to_strings(&input, &mat);
        all_matches.push(serde_json::json!({
            "matches": matches,
            "index": mat.start()
        }));
    }

    let result = serde_json::to_string(&all_matches).unwrap_or_else(|_| "[]".to_string());
    Ok(Value::string(JsString::intern(&result)))
}

/// RegExp.prototype[Symbol.replace](string, replacement) - used by String.prototype.replace
fn regexp_replace(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, String> {
    let (pattern, flags) = parse_regex_args(args).ok_or("Invalid RegExp")?;
    let input = args
        .get(2)
        .and_then(|v| v.as_string())
        .ok_or("replace requires a string argument")?;
    let replacement = args
        .get(3)
        .and_then(|v| v.as_string())
        .map(|s| s.to_string())
        .unwrap_or_default();

    let regex = build_regex(&pattern, &flags)?;
    let is_global = flags.contains('g');

    // Convert JS replacement patterns to Rust regex patterns
    // JS uses $1, $2 etc; Rust uses $1, $2 etc (same!)
    // JS uses $& for full match; Rust uses $0
    let rust_replacement = replacement
        .replace("$&", "$0")
        .replace("$`", "") // $` not supported in Rust regex
        .replace("$'", ""); // $' not supported in Rust regex

    let result = if is_global {
        regex.replace_all(input.as_str(), rust_replacement.as_str())
    } else {
        regex.replace(input.as_str(), rust_replacement.as_str())
    };

    Ok(Value::string(JsString::intern(&result)))
}

/// RegExp.prototype[Symbol.search](string) - used by String.prototype.search
/// Returns index of first match, or -1 if not found
fn regexp_search(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, String> {
    let (pattern, flags) = parse_regex_args(args).ok_or("Invalid RegExp")?;
    let input = args
        .get(2)
        .and_then(|v| v.as_string())
        .ok_or("search requires a string argument")?;

    let regex = build_regex(&pattern, &flags)?;

    match find_first(&regex, &input, &flags, 0) {
        Some(mat) => Ok(Value::int32(mat.start() as i32)),
        None => Ok(Value::int32(-1)),
    }
}

/// RegExp.prototype[Symbol.split](string, limit) - used by String.prototype.split
fn regexp_split(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, String> {
    let (pattern, flags) = parse_regex_args(args).ok_or("Invalid RegExp")?;
    let input = args
        .get(2)
        .and_then(|v| v.as_string())
        .ok_or("split requires a string argument")?;
    let limit = args.get(3).and_then(|v| v.as_int32()).map(|n| n as usize);

    let regex = build_regex(&pattern, &flags)?;

    let mut parts = Vec::new();
    let mut last_end = 0;
    let input_len = input.len_utf16();

    for mat in find_all(&regex, &input, &flags) {
        if let Some(lim) = limit {
            if parts.len() >= lim {
                break;
            }
        }
        let start = mat.start();
        if start >= last_end {
            parts.push(slice_utf16(&input, last_end..start));
        }
        last_end = mat.end();
    }

    if limit.map(|l| parts.len() < l).unwrap_or(true) {
        parts.push(slice_utf16(&input, last_end..input_len));
    }

    let result = serde_json::to_string(&parts).unwrap_or_else(|_| "[]".to_string());
    Ok(Value::string(JsString::intern(&result)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_regexp_escape() {
        let mm = Arc::new(memory::MemoryManager::test());
        let args = vec![Value::string(JsString::intern("hello.world*"))];
        let result = regexp_escape(&args, mm).unwrap();
        let s = result.as_string().unwrap().to_string();
        assert_eq!(s, r"hello\.world\*");
    }

    #[test]
    fn test_regexp_test() {
        let mm = Arc::new(memory::MemoryManager::test());
        let args = vec![
            Value::string(JsString::intern(r"\d+")),
            Value::string(JsString::intern("")),
            Value::string(JsString::intern("abc123def")),
        ];
        let result = regexp_test(&args, mm).unwrap();
        assert_eq!(result.as_boolean(), Some(true));
    }

    #[test]
    fn test_regexp_test_no_match() {
        let mm = Arc::new(memory::MemoryManager::test());
        let args = vec![
            Value::string(JsString::intern(r"\d+")),
            Value::string(JsString::intern("")),
            Value::string(JsString::intern("abcdef")),
        ];
        let result = regexp_test(&args, mm).unwrap();
        assert_eq!(result.as_boolean(), Some(false));
    }

    #[test]
    fn test_regexp_test_case_insensitive() {
        let mm = Arc::new(memory::MemoryManager::test());
        let args = vec![
            Value::string(JsString::intern("hello")),
            Value::string(JsString::intern("i")),
            Value::string(JsString::intern("HELLO WORLD")),
        ];
        let result = regexp_test(&args, mm).unwrap();
        assert_eq!(result.as_boolean(), Some(true));
    }

    #[test]
    fn test_regexp_search() {
        let mm = Arc::new(memory::MemoryManager::test());
        let args = vec![
            Value::string(JsString::intern(r"\d+")),
            Value::string(JsString::intern("")),
            Value::string(JsString::intern("abc123def")),
        ];
        let result = regexp_search(&args, mm).unwrap();
        assert_eq!(result.as_int32(), Some(3));
    }

    #[test]
    fn test_regexp_search_not_found() {
        let mm = Arc::new(memory::MemoryManager::test());
        let args = vec![
            Value::string(JsString::intern(r"\d+")),
            Value::string(JsString::intern("")),
            Value::string(JsString::intern("abcdef")),
        ];
        let result = regexp_search(&args, mm).unwrap();
        assert_eq!(result.as_int32(), Some(-1));
    }

    #[test]
    fn test_regexp_replace() {
        let mm = Arc::new(memory::MemoryManager::test());
        let args = vec![
            Value::string(JsString::intern(r"\d+")),
            Value::string(JsString::intern("")),
            Value::string(JsString::intern("abc123def")),
            Value::string(JsString::intern("XXX")),
        ];
        let result = regexp_replace(&args, mm).unwrap();
        let s = result.as_string().unwrap().to_string();
        assert_eq!(s, "abcXXXdef");
    }

    #[test]
    fn test_regexp_replace_global() {
        let mm = Arc::new(memory::MemoryManager::test());
        let args = vec![
            Value::string(JsString::intern(r"\d+")),
            Value::string(JsString::intern("g")),
            Value::string(JsString::intern("a1b2c3")),
            Value::string(JsString::intern("X")),
        ];
        let result = regexp_replace(&args, mm).unwrap();
        let s = result.as_string().unwrap().to_string();
        assert_eq!(s, "aXbXcX");
    }

    #[test]
    fn test_regexp_split() {
        let mm = Arc::new(memory::MemoryManager::test());
        let args = vec![
            Value::string(JsString::intern(r"\s+")),
            Value::string(JsString::intern("")),
            Value::string(JsString::intern("hello world foo")),
        ];
        let result = regexp_split(&args, mm).unwrap();
        let s = result.as_string().unwrap().to_string();
        assert!(s.contains("hello"));
        assert!(s.contains("world"));
        assert!(s.contains("foo"));
    }

    #[test]
    fn test_regexp_to_string() {
        let mm = Arc::new(memory::MemoryManager::test());
        let args = vec![
            Value::string(JsString::intern(r"\d+")),
            Value::string(JsString::intern("gi")),
        ];
        let result = regexp_to_string(&args, mm).unwrap();
        let s = result.as_string().unwrap().to_string();
        assert_eq!(s, r"/\d+/gi");
    }

    #[test]
    fn test_regexp_flags() {
        let mm = Arc::new(memory::MemoryManager::test());
        let args = vec![
            Value::string(JsString::intern("test")),
            Value::string(JsString::intern("gim")),
        ];

        assert_eq!(
            regexp_global(&args, mm.clone()).unwrap().as_boolean(),
            Some(true)
        );
        assert_eq!(
            regexp_ignore_case(&args, mm.clone()).unwrap().as_boolean(),
            Some(true)
        );
        assert_eq!(
            regexp_multiline(&args, mm.clone()).unwrap().as_boolean(),
            Some(true)
        );
        assert_eq!(
            regexp_dot_all(&args, mm.clone()).unwrap().as_boolean(),
            Some(false)
        );
        assert_eq!(regexp_sticky(&args, mm).unwrap().as_boolean(), Some(false));
    }

    #[test]
    fn test_regexp_exec() {
        let mm = Arc::new(memory::MemoryManager::test());
        let args = vec![
            Value::string(JsString::intern(r"(\d+)")),
            Value::string(JsString::intern("")),
            Value::string(JsString::intern("abc123def")),
        ];
        let result = regexp_exec(&args, mm).unwrap();
        let s = result.as_string().unwrap().to_string();
        assert!(s.contains("123"));
        assert!(s.contains("\"index\":3"));
    }

    #[test]
    fn test_regexp_exec_no_match() {
        let mm = Arc::new(memory::MemoryManager::test());
        let args = vec![
            Value::string(JsString::intern(r"\d+")),
            Value::string(JsString::intern("")),
            Value::string(JsString::intern("abcdef")),
        ];
        let result = regexp_exec(&args, mm).unwrap();
        assert!(result.is_null());
    }
}
