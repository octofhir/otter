//! §19.2.1 eval(x) — global eval function and URI encoding/decoding functions.
//!
//! Spec: <https://tc39.es/ecma262/#sec-eval-x>

use crate::descriptors::{NativeFunctionDescriptor, VmNativeCallError};
use crate::interpreter::RuntimeState;
use crate::value::RegisterValue;

const URI_INTERRUPT_POLL_INTERVAL: usize = 4096;

#[derive(Debug)]
enum UriCodecError {
    Uri(String),
    Interrupted,
}

impl From<String> for UriCodecError {
    fn from(message: String) -> Self {
        Self::Uri(message)
    }
}

impl From<&'static str> for UriCodecError {
    fn from(message: &'static str) -> Self {
        Self::Uri(message.into())
    }
}

fn poll_uri_interrupt(runtime: Option<&RuntimeState>, index: usize) -> Result<(), UriCodecError> {
    if index.is_multiple_of(URI_INTERRUPT_POLL_INTERVAL)
        && runtime.is_some_and(RuntimeState::is_execution_interrupted)
    {
        return Err(UriCodecError::Interrupted);
    }
    Ok(())
}

fn uri_codec_result(
    runtime: &mut RuntimeState,
    result: Result<String, UriCodecError>,
) -> Result<RegisterValue, VmNativeCallError> {
    match result {
        Ok(output) => {
            let handle = runtime.alloc_string(output);
            Ok(RegisterValue::from_object_handle(handle.0))
        }
        Err(UriCodecError::Uri(message)) => Err(uri_error(runtime, &message)),
        Err(UriCodecError::Interrupted) => {
            runtime.check_interrupt()?;
            Err(VmNativeCallError::Internal("execution interrupted".into()))
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  §19.2.1 eval(x)
//  Spec: <https://tc39.es/ecma262/#sec-eval-x>
// ═══════════════════════════════════════════════════════════════════════════

/// §19.2.1 eval(x)
///
/// Indirect eval: evaluates `x` as a Script in the global scope.
/// If `x` is not a string, returns `x` unchanged.
///
/// Spec: <https://tc39.es/ecma262/#sec-eval-x>
fn global_eval(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let x = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);

    // §19.2.1 Step 1: If x is not a String, return x.
    let Some(source) = runtime.value_as_string(x) else {
        return Ok(x);
    };

    // §19.2.1 Step 2: PerformEval(x, false, false)
    // direct = false (indirect eval), strictCaller = false.
    runtime.eval_source(&source, false, false)
}

// ═══════════════════════════════════════════════════════════════════════════
//  §19.2.6.1 encodeURI(uriString)
//  Spec: <https://tc39.es/ecma262/#sec-encodeuri-uristring>
// ═══════════════════════════════════════════════════════════════════════════

/// §19.2.6.1 encodeURI(uriString)
///
/// Encodes a complete URI, preserving characters that have special meaning
/// in a URI (;/?:@&=+$,#) and unreserved marks (-_.!~*'()).
///
/// Spec: <https://tc39.es/ecma262/#sec-encodeuri-uristring>
fn global_encode_uri(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let arg = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let s = runtime
        .js_to_string(arg)
        .map_err(|e| VmNativeCallError::Internal(format!("encodeURI: {e}").into()))?;

    // Characters NOT to encode (URI reserved + unreserved per RFC 3986 + '#').
    const URI_UNESCAPED_RESERVED: &str =
        "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_.!~*'();/?:@&=+$,#";

    uri_codec_result(
        runtime,
        encode_uri_component_impl(&s, URI_UNESCAPED_RESERVED, Some(runtime)),
    )
}

// ═══════════════════════════════════════════════════════════════════════════
//  §19.2.6.2 encodeURIComponent(uriComponent)
//  Spec: <https://tc39.es/ecma262/#sec-encodeuricomponent-uricomponent>
// ═══════════════════════════════════════════════════════════════════════════

/// §19.2.6.2 encodeURIComponent(uriComponent)
///
/// Encodes a URI component, encoding all characters except unreserved marks.
///
/// Spec: <https://tc39.es/ecma262/#sec-encodeuricomponent-uricomponent>
fn global_encode_uri_component(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let arg = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let s = runtime
        .js_to_string(arg)
        .map_err(|e| VmNativeCallError::Internal(format!("encodeURIComponent: {e}").into()))?;

    const URI_UNRESERVED: &str =
        "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_.!~*'()";

    uri_codec_result(
        runtime,
        encode_uri_component_impl(&s, URI_UNRESERVED, Some(runtime)),
    )
}

// ═══════════════════════════════════════════════════════════════════════════
//  §19.2.6.3 decodeURI(encodedURI)
//  Spec: <https://tc39.es/ecma262/#sec-decodeuri-encodeduri>
// ═══════════════════════════════════════════════════════════════════════════

/// §19.2.6.3 decodeURI(encodedURI)
///
/// Decodes a complete URI, leaving URI-reserved character escapes intact.
///
/// Spec: <https://tc39.es/ecma262/#sec-decodeuri-encodeduri>
fn global_decode_uri(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let arg = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let s = runtime
        .js_to_string(arg)
        .map_err(|e| VmNativeCallError::Internal(format!("decodeURI: {e}").into()))?;

    // Characters whose escapes should NOT be decoded (URI reserved set).
    const RESERVED_URI_SET: &str = ";/?:@&=+$,#";

    uri_codec_result(
        runtime,
        decode_uri_impl(&s, RESERVED_URI_SET, Some(runtime)),
    )
}

// ═══════════════════════════════════════════════════════════════════════════
//  §19.2.6.4 decodeURIComponent(encodedURIComponent)
//  Spec: <https://tc39.es/ecma262/#sec-decodeuricomponent-encodeduricomponent>
// ═══════════════════════════════════════════════════════════════════════════

/// §19.2.6.4 decodeURIComponent(encodedURIComponent)
///
/// Decodes a URI component, decoding all percent-encoded sequences.
///
/// Spec: <https://tc39.es/ecma262/#sec-decodeuricomponent-encodeduricomponent>
fn global_decode_uri_component(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let arg = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let s = runtime
        .js_to_string(arg)
        .map_err(|e| VmNativeCallError::Internal(format!("decodeURIComponent: {e}").into()))?;

    // No reserved set — decode everything.
    uri_codec_result(runtime, decode_uri_impl(&s, "", Some(runtime)))
}

// ═══════════════════════════════════════════════════════════════════════════
//  Helpers
// ═══════════════════════════════════════════════════════════════════════════

/// §19.2.6.1.2 Encode(string, unescapedSet)
///
/// Percent-encodes characters NOT in `unescaped_set` using UTF-8 bytes.
/// Handles surrogate pairs per the spec.
///
/// Spec: <https://tc39.es/ecma262/#sec-encode>
fn encode_uri_component_impl(
    s: &str,
    unescaped_set: &str,
    runtime: Option<&RuntimeState>,
) -> Result<String, UriCodecError> {
    let mut result = String::with_capacity(s.len());
    for (index, ch) in s.chars().enumerate() {
        poll_uri_interrupt(runtime, index)?;
        if unescaped_set.contains(ch) {
            result.push(ch);
        } else {
            // Encode each UTF-8 byte as %XX.
            let mut buf = [0u8; 4];
            let encoded = ch.encode_utf8(&mut buf);
            for &byte in encoded.as_bytes() {
                result.push('%');
                result.push(hex_digit(byte >> 4));
                result.push(hex_digit(byte & 0x0F));
            }
        }
    }
    Ok(result)
}

/// §19.2.6.1.3 Decode(string, reservedSet)
///
/// Decodes percent-encoded sequences. Sequences that decode to characters
/// in `reserved_set` are left as-is.
///
/// Spec: <https://tc39.es/ecma262/#sec-decode>
fn decode_uri_impl(
    s: &str,
    reserved_set: &str,
    runtime: Option<&RuntimeState>,
) -> Result<String, UriCodecError> {
    let bytes = s.as_bytes();
    let len = bytes.len();
    let mut result = String::with_capacity(len);
    let mut i = 0;

    while i < len {
        poll_uri_interrupt(runtime, i)?;
        if bytes[i] == b'%' {
            // Decode one or more percent-encoded bytes.
            let start = i;
            let first_byte = decode_hex_pair(bytes, i + 1, len)?;
            i += 3;

            if first_byte & 0x80 == 0 {
                // Single-byte character.
                let ch = first_byte as char;
                if reserved_set.contains(ch) {
                    // Leave the escape sequence intact.
                    result.push_str(&s[start..i]);
                } else {
                    result.push(ch);
                }
            } else {
                // Multi-byte UTF-8 sequence.
                let n = leading_ones(first_byte);
                if !(2..=4).contains(&n) {
                    return Err("URIError: malformed URI sequence".into());
                }
                let mut utf8_bytes = vec![first_byte];
                for _ in 1..n {
                    if i >= len || bytes[i] != b'%' {
                        return Err("URIError: malformed URI sequence".into());
                    }
                    let continuation = decode_hex_pair(bytes, i + 1, len)?;
                    if continuation & 0xC0 != 0x80 {
                        return Err("URIError: malformed URI sequence".into());
                    }
                    utf8_bytes.push(continuation);
                    i += 3;
                }
                let decoded = std::str::from_utf8(&utf8_bytes)
                    .map_err(|_| UriCodecError::Uri("URIError: malformed URI sequence".into()))?;
                let ch = decoded
                    .chars()
                    .next()
                    .ok_or("URIError: malformed URI sequence")?;
                if reserved_set.contains(ch) {
                    result.push_str(&s[start..i]);
                } else {
                    result.push(ch);
                }
            }
        } else {
            result.push(bytes[i] as char);
            i += 1;
        }
    }

    Ok(result)
}

fn decode_hex_pair(bytes: &[u8], start: usize, len: usize) -> Result<u8, UriCodecError> {
    if start + 1 >= len {
        return Err("URIError: malformed URI sequence".into());
    }
    let hi = hex_value(bytes[start]).ok_or("URIError: malformed URI sequence")?;
    let lo = hex_value(bytes[start + 1]).ok_or("URIError: malformed URI sequence")?;
    Ok((hi << 4) | lo)
}

fn hex_value(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn hex_digit(nibble: u8) -> char {
    match nibble {
        0..=9 => (b'0' + nibble) as char,
        10..=15 => (b'A' + nibble - 10) as char,
        _ => unreachable!(),
    }
}

fn leading_ones(byte: u8) -> usize {
    (!byte).leading_zeros() as usize
}

fn uri_error(runtime: &mut RuntimeState, message: &str) -> VmNativeCallError {
    let prototype = runtime.intrinsics().uri_error_prototype;
    let handle = runtime.alloc_object_with_prototype(Some(prototype));
    let msg = runtime.alloc_string(message);
    let msg_prop = runtime.intern_property_name("message");
    runtime
        .objects_mut()
        .set_property(handle, msg_prop, RegisterValue::from_object_handle(msg.0))
        .ok();
    VmNativeCallError::Thrown(RegisterValue::from_object_handle(handle.0))
}

/// Returns the descriptors for the global eval and URI functions.
///
/// §19.2 Function Properties of the Global Object.
/// Spec: <https://tc39.es/ecma262/#sec-function-properties-of-the-global-object>
pub(super) fn global_eval_and_uri_bindings() -> Vec<NativeFunctionDescriptor> {
    vec![
        NativeFunctionDescriptor::method("eval", 1, global_eval),
        NativeFunctionDescriptor::method("encodeURI", 1, global_encode_uri),
        NativeFunctionDescriptor::method("encodeURIComponent", 1, global_encode_uri_component),
        NativeFunctionDescriptor::method("decodeURI", 1, global_decode_uri),
        NativeFunctionDescriptor::method("decodeURIComponent", 1, global_decode_uri_component),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_uri_basic() {
        let unescaped =
            "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_.!~*'();/?:@&=+$,#";
        let result = encode_uri_component_impl("hello world", unescaped, None).unwrap();
        assert_eq!(result, "hello%20world");
    }

    #[test]
    fn encode_uri_component_basic() {
        let unescaped = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_.!~*'()";
        let result = encode_uri_component_impl("hello world&foo=bar", unescaped, None).unwrap();
        assert_eq!(result, "hello%20world%26foo%3Dbar");
    }

    #[test]
    fn decode_uri_basic() {
        let result = decode_uri_impl("hello%20world", "", None).unwrap();
        assert_eq!(result, "hello world");
    }

    #[test]
    fn decode_uri_preserves_reserved() {
        // decodeURI should NOT decode reserved characters like #
        let result = decode_uri_impl("hello%23world", ";/?:@&=+$,#", None).unwrap();
        assert_eq!(result, "hello%23world");
    }

    #[test]
    fn decode_uri_component_decodes_reserved() {
        // decodeURIComponent decodes everything
        let result = decode_uri_impl("hello%23world", "", None).unwrap();
        assert_eq!(result, "hello#world");
    }

    #[test]
    fn decode_uri_multibyte_utf8() {
        let result = decode_uri_impl("%C3%A9", "", None).unwrap();
        assert_eq!(result, "é");
    }

    #[test]
    fn decode_uri_malformed() {
        assert!(decode_uri_impl("%G0", "", None).is_err());
        assert!(decode_uri_impl("%", "", None).is_err());
        assert!(decode_uri_impl("%0", "", None).is_err());
    }

    #[test]
    fn encode_decode_roundtrip() {
        let unescaped = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_.!~*'()";
        let original = "こんにちは世界 hello=world&foo";
        let encoded = encode_uri_component_impl(original, unescaped, None).unwrap();
        let decoded = decode_uri_impl(&encoded, "", None).unwrap();
        assert_eq!(decoded, original);
    }
}
