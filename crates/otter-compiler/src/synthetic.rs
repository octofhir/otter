//! Synthetic names and literal-key helpers shared by module and object lowering.
//!
//! # Contents
//! - module/import synthetic name builders
//! - UTF-16 string decoding
//! - numeric literal property-key formatting
//!
//! # Invariants
//! - Generated names must stay outside user binding syntax.
//!
//! # See also
//! - `module_state` and `expr`

/// Synthetic binding name used to capture the `module_env`
/// JsObject through inner-function `resolve_capture` cascades.
/// Inner functions that mutate a module-level export reach the
/// outer module-init's `module_env` cell via this name.
pub(crate) fn module_env_synthetic_name() -> String {
    "__otter_module_env".to_string()
}

/// Synthetic binding name for the `import_meta` JsObject.
pub(crate) fn import_meta_synthetic_name() -> String {
    "__otter_import_meta".to_string()
}

/// Synthetic binding name for an import-record at the given
/// outer-frame upvalue index. Distinct names per-record let inner
/// functions cascade each independently.
pub(crate) fn import_record_synthetic_name(record_uv: u16) -> String {
    format!("__otter_import_record_{record_uv}")
}

/// Decode oxc's lossy lone-surrogate encoding back into raw WTF-16
/// code units. When `StringLiteral::lone_surrogates` is set, oxc
/// stores each lone surrogate as `\u{FFFD}XXXX` (four lowercase hex
/// digits) and the literal U+FFFD as `\u{FFFD}fffd`. This decoder
/// reverses both encodings so the runtime sees the source-fidelity
/// code units expected by §6.1.4
/// [`The String Type`](https://tc39.es/ecma262/#sec-ecmascript-language-types-string-type).
pub(crate) fn decode_lone_surrogate_string(value: &str) -> Vec<u16> {
    let mut out: Vec<u16> = Vec::with_capacity(value.len());
    let mut iter = value.chars().peekable();
    while let Some(c) = iter.next() {
        if c == '\u{FFFD}' {
            // Followed by four lowercase hex digits encoding a u16.
            let mut hex = [0u8; 4];
            let mut count = 0;
            for slot in &mut hex {
                match iter.peek() {
                    Some(&h) if h.is_ascii_hexdigit() => {
                        *slot = h as u8;
                        iter.next();
                        count += 1;
                    }
                    _ => break,
                }
            }
            if count == 4 {
                let s = std::str::from_utf8(&hex).unwrap();
                let unit = u16::from_str_radix(s, 16).unwrap();
                out.push(unit);
                continue;
            }
            // Malformed (shouldn't happen if `lone_surrogates`
            // signal is honoured) — fall back to literal U+FFFD.
            out.push(0xFFFD);
            for h in &hex[..count] {
                out.push(*h as u16);
            }
        } else {
            let mut buf = [0u16; 2];
            for u in c.encode_utf16(&mut buf).iter() {
                out.push(*u);
            }
        }
    }
    out
}

/// Format a `NumericLiteral`'s value as a property key per
/// §6.1.7.1 ToString(Number). Integer values produce the bare
/// integer string ("1" not "1.0"); other finite numbers go
/// through Rust's default f64 formatter.
pub(crate) fn numeric_literal_to_property_key(n: f64) -> String {
    if n.is_finite() && n.fract() == 0.0 && n.abs() < 1e21 {
        format!("{}", n as i64)
    } else {
        format!("{n}")
    }
}
