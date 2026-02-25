use crate::gc::GcRef;
use crate::object::{JsObject, PropertyAttributes, PropertyDescriptor, PropertyKey};
use crate::string::JsString;
use crate::value::Value;
use regress::{Flags, Regex};
use std::sync::Arc;

/// JavaScript RegExp object
#[derive(Debug)]
pub struct JsRegExp {
    /// The Ordinary Object part (properties like lastIndex) - GC-managed
    pub object: GcRef<JsObject>,
    /// The regex pattern
    pub pattern: String,
    /// The regex flags
    pub flags: String,
    /// Whether this regex uses Unicode (u or v flags)
    pub unicode: bool,
    /// Capture group names by capture index (1-based slots stored 0-based).
    /// `None` for unnamed capturing groups.
    pub capture_group_names: Vec<Option<String>>,
    /// Fast-path fallback for non-unicode literal patterns with astral chars.
    pub fallback_literal_utf16: Option<Vec<u16>>,
    /// The compiled Rust regex (if compilation succeeded)
    pub native_regex: Option<Regex>,
}

impl otter_vm_gc::GcTraceable for JsRegExp {
    const NEEDS_TRACE: bool = true;
    const TYPE_ID: u8 = otter_vm_gc::object::tags::REGEXP;
    fn trace(&self, tracer: &mut dyn FnMut(*const otter_vm_gc::GcHeader)) {
        // Trace the object field (GC-managed)
        tracer(self.object.header() as *const _);
    }
}

impl JsRegExp {
    /// Create a new JsRegExp
    pub fn new(
        pattern: String,
        flags: String,
        proto: Option<GcRef<JsObject>>,
        memory_manager: Arc<crate::memory::MemoryManager>,
    ) -> Self {
        let proto_value = proto.map(Value::object).unwrap_or_else(Value::null);
        let object = GcRef::new(JsObject::new(proto_value, memory_manager));
        object.define_property(
            PropertyKey::string("lastIndex"),
            PropertyDescriptor::data_with_attrs(
                Value::number(0.0),
                PropertyAttributes {
                    writable: true,
                    enumerable: false,
                    configurable: false,
                },
            ),
        );
        // Per spec, regex instances do NOT have own properties for flags, source,
        // global, etc. These are accessor getters on RegExp.prototype that read
        // from the [[RegExpMatcher]] internal slot (our JsRegExp struct fields).
        // Only lastIndex is an own data property.
        let parsed_flags = Flags::from(flags.as_str());
        let unicode = parsed_flags.unicode || parsed_flags.unicode_sets;
        let capture_group_names = parse_capture_group_names(&pattern);
        let engine_pattern = compile_pattern_for_regress(&pattern, &parsed_flags);
        let fallback_literal_utf16 = compute_literal_utf16_fallback(&pattern, &parsed_flags);
        let native_regex = Regex::with_flags(&engine_pattern, parsed_flags).ok();

        Self {
            object,
            pattern,
            flags,
            unicode,
            capture_group_names,
            fallback_literal_utf16,
            native_regex,
        }
    }

    pub fn memory_manager(&self) -> &Arc<crate::memory::MemoryManager> {
        self.object.memory_manager()
    }

    /// Execute the regex on a string
    pub fn exec(&self, input: &JsString, start: usize) -> Option<regress::Match> {
        let re = self.native_regex.as_ref()?;
        if self.unicode {
            re.find_from_utf16(input.as_utf16(), start).next()
        } else {
            re.find_from_ucs2(input.as_utf16(), start).next()
        }
    }
}

/// Convert non-BMP literals into surrogate-pair escapes for non-unicode matching.
///
/// JS non-`u`/`v` regexes operate on UCS-2 code units. Regress parses Rust UTF-8
/// source scalars; this rewrite preserves JS behavior for astral literals like `𠮷`.
pub(crate) fn compile_pattern_for_regress(pattern: &str, flags: &Flags) -> String {
    if flags.unicode || flags.unicode_sets {
        return pattern.to_string();
    }

    let mut result = String::with_capacity(pattern.len());
    let chars: Vec<char> = pattern.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        if ch == '\\' && i + 1 < chars.len() {
            let next = chars[i + 1];
            match next {
                'd'
                | 'w'
                | 's'
                | 'D'
                | 'W'
                | 'S'
                | 'b'
                | 'B'
                | 'n'
                | 'r'
                | 't'
                | 'v'
                | 'f'
                | '0'
                | '1'..='9'
                | '\\'
                | '/'
                | '('
                | ')'
                | '['
                | ']'
                | '{'
                | '}'
                | '.'
                | '^'
                | '$'
                | '*'
                | '+'
                | '?'
                | '|' => {
                    result.push('\\');
                    result.push(next);
                    i += 2;
                }
                'x' => {
                    if i + 3 < chars.len()
                        && chars[i + 2].is_ascii_hexdigit()
                        && chars[i + 3].is_ascii_hexdigit()
                    {
                        result.push('\\');
                        result.push('x');
                        i += 2;
                    } else {
                        // Annex B: incomplete \x is identity escape for 'x'
                        result.push('x');
                        i += 2;
                    }
                }
                'u' => {
                    if i + 5 < chars.len()
                        && chars[i + 2..i + 6].iter().all(|c| c.is_ascii_hexdigit())
                    {
                        result.push('\\');
                        result.push('u');
                        i += 2;
                    } else {
                        // Annex B: incomplete \u is identity escape for 'u'
                        result.push('u');
                        i += 2;
                    }
                }
                'c' => {
                    if i + 2 < chars.len() && chars[i + 2].is_ascii_alphabetic() {
                        result.push('\\');
                        result.push('c');
                        i += 2;
                    } else {
                        // Annex B: \c followed by non-letter match literal "\c"
                        result.push_str("\\\\c");
                        i += 2;
                    }
                }
                _ => {
                    // Annex B: identity escape for any other character
                    if next as u32 > 0xFFFF {
                        let mut buf = [0u16; 2];
                        next.encode_utf16(&mut buf);
                        result.push_str(&format!("\\u{:04x}\\u{:04x}", buf[0], buf[1]));
                    } else {
                        result.push(next);
                    }
                    i += 2;
                }
            }
        } else if ch as u32 > 0xFFFF {
            // Surrogateify astral char in non-unicode mode
            let mut buf = [0u16; 2];
            ch.encode_utf16(&mut buf);
            result.push_str(&format!("\\u{:04x}\\u{:04x}", buf[0], buf[1]));
            i += 1;
        } else {
            result.push(ch);
            i += 1;
        }
    }
    result
}

fn is_plain_literal_pattern(pattern: &str) -> bool {
    // Keep this strict: fallback is only for true literal patterns.
    !pattern.is_empty()
        && !pattern.contains('\\')
        && !pattern.chars().any(|c| {
            matches!(
                c,
                '.' | '^' | '$' | '*' | '+' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '|'
            )
        })
}

pub(crate) fn compute_literal_utf16_fallback(pattern: &str, flags: &Flags) -> Option<Vec<u16>> {
    if flags.unicode || flags.unicode_sets || !is_plain_literal_pattern(pattern) {
        return None;
    }
    let has_astral = pattern.chars().any(|ch| (ch as u32) > 0xFFFF);
    if !has_astral {
        return None;
    }
    Some(pattern.encode_utf16().collect())
}

pub(crate) fn parse_capture_group_names(pattern: &str) -> Vec<Option<String>> {
    let chars: Vec<char> = pattern.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    let mut in_class = false;

    while i < chars.len() {
        let ch = chars[i];
        if ch == '\\' {
            i += 2;
            continue;
        }
        if in_class {
            if ch == ']' {
                in_class = false;
            }
            i += 1;
            continue;
        }
        if ch == '[' {
            in_class = true;
            i += 1;
            continue;
        }
        if ch != '(' {
            i += 1;
            continue;
        }

        // Group start
        if i + 1 < chars.len() && chars[i + 1] == '?' {
            if i + 2 < chars.len() {
                match chars[i + 2] {
                    ':' | '=' | '!' => {
                        // Non-capturing / lookahead
                    }
                    '<' => {
                        // Named capture (?<name>...) vs lookbehind (?<= / ?<!)
                        if i + 3 < chars.len() && (chars[i + 3] == '=' || chars[i + 3] == '!') {
                            // lookbehind, non-capturing
                        } else {
                            let mut j = i + 3;
                            let mut name = String::new();
                            while j < chars.len() && chars[j] != '>' {
                                name.push(chars[j]);
                                j += 1;
                            }
                            if !name.is_empty() && j < chars.len() && chars[j] == '>' {
                                out.push(Some(name));
                            } else {
                                out.push(None);
                            }
                        }
                    }
                    _ => {
                        // Inline modifier group; non-capturing.
                    }
                }
            }
        } else {
            out.push(None);
        }
        i += 1;
    }

    out
}
