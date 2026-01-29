//! Global functions
//!
//! Provides legacy global functions:
//! - escape(string) - URL encoding (Annex B)
//! - unescape(string) - URL decoding (Annex B)

use otter_vm_runtime::{Op, op_sync};
use serde_json::{Value as JsonValue, json};

/// Get global function ops for extension registration
pub fn ops() -> Vec<Op> {
    vec![
        op_sync("__global_escape", global_escape),
        op_sync("__global_unescape", global_unescape),
    ]
}

fn get_string(args: &[JsonValue], index: usize) -> String {
    args.get(index)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

/// Annex B escape() function
/// Encodes a string for use in a URL, but preserves certain ASCII characters
/// NOTE: Works with UTF-16 code units, not code points (for emoji/surrogate pairs compatibility)
fn global_escape(args: &[JsonValue]) -> Result<JsonValue, String> {
    let s = get_string(args, 0);
    let mut result = String::new();

    // Convert to UTF-16 to match JavaScript string behavior
    let utf16_units: Vec<u16> = s.encode_utf16().collect();

    for &code_unit in &utf16_units {
        let ch = code_unit as u32;

        // Characters that are NOT escaped: A-Z a-z 0-9 @ * _ + - . /
        // Check if it's an ASCII character that should not be escaped
        if (ch >= 0x41 && ch <= 0x5A)  // A-Z
            || (ch >= 0x61 && ch <= 0x7A)  // a-z
            || (ch >= 0x30 && ch <= 0x39)  // 0-9
            || ch == 0x40  // @
            || ch == 0x2A  // *
            || ch == 0x5F  // _
            || ch == 0x2B  // +
            || ch == 0x2D  // -
            || ch == 0x2E  // .
            || ch == 0x2F  // /
        {
            // Safe to convert single UTF-16 unit to char for ASCII
            if let Some(c) = char::from_u32(ch) {
                result.push(c);
            }
        } else if ch < 256 {
            // Single-byte character: %XX
            result.push_str(&format!("%{:02X}", ch));
        } else {
            // Multi-byte character (including surrogate pairs): %uXXXX
            result.push_str(&format!("%u{:04X}", code_unit));
        }
    }

    Ok(json!(result))
}

/// Annex B unescape() function
/// Decodes a string encoded by escape()
/// NOTE: Handles UTF-16 surrogate pairs correctly
fn global_unescape(args: &[JsonValue]) -> Result<JsonValue, String> {
    let s = get_string(args, 0);
    let chars: Vec<char> = s.chars().collect();
    let mut utf16_units: Vec<u16> = Vec::new();
    let mut i = 0;

    while i < chars.len() {
        if chars[i] == '%' && i + 1 < chars.len() {
            // Check for %uXXXX format (Unicode escape)
            if i + 5 < chars.len() && chars[i + 1] == 'u' {
                let hex = &chars[i + 2..i + 6];
                let hex_str: String = hex.iter().collect();
                if let Ok(code) = u16::from_str_radix(&hex_str, 16) {
                    // Store as UTF-16 code unit (might be part of surrogate pair)
                    utf16_units.push(code);
                    i += 6;
                    continue;
                }
                // If parsing failed, treat as literal character
                utf16_units.push('%' as u16);
                i += 1;
            }
            // Check for %XX format (byte escape)
            else if i + 2 < chars.len() {
                let hex = &chars[i + 1..i + 3];
                let hex_str: String = hex.iter().collect();
                if let Ok(code) = u8::from_str_radix(&hex_str, 16) {
                    // Single byte, store as UTF-16
                    utf16_units.push(code as u16);
                    i += 3;
                    continue;
                }
                // If parsing failed, treat as literal character
                utf16_units.push('%' as u16);
                i += 1;
            } else {
                // Not enough characters for escape sequence
                utf16_units.push('%' as u16);
                i += 1;
            }
        } else {
            // Regular character
            utf16_units.push(chars[i] as u16);
            i += 1;
        }
    }

    // Decode UTF-16 to Rust String (handles surrogate pairs automatically)
    let result = String::from_utf16_lossy(&utf16_units);
    Ok(json!(result))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_escape() {
        let result = global_escape(&[json!("Hello World!")]).unwrap();
        println!("escape('Hello World!') = {:?}", result);
        assert_eq!(result, json!("Hello%20World%21"));

        let result = global_escape(&[json!("abc123")]).unwrap();
        println!("escape('abc123') = {:?}", result);
        assert_eq!(result, json!("abc123"));

        let result = global_escape(&[json!("test@example.com")]).unwrap();
        println!("escape('test@example.com') = {:?}", result);
        assert_eq!(result, json!("test@example.com"));

        // Test emoji (surrogate pair)
        let result = global_escape(&[json!("😀")]).unwrap();
        println!("escape('😀') = {:?}", result);
        assert_eq!(result, json!("%uD83D%uDE00")); // Surrogate pair for emoji

        // Test characters that should not be escaped
        let result = global_escape(&[json!("@*_+-./ ")]).unwrap();
        println!("escape('@*_+-./ ') = {:?}", result);
        assert_eq!(result, json!("@*_+-./%20"));
    }

    #[test]
    fn test_unescape() {
        let result = global_unescape(&[json!("Hello%20World%21")]).unwrap();
        assert_eq!(result, json!("Hello World!"));

        let result = global_unescape(&[json!("abc123")]).unwrap();
        assert_eq!(result, json!("abc123"));

        let result = global_unescape(&[json!("test@example.com")]).unwrap();
        assert_eq!(result, json!("test@example.com"));

        let result = global_unescape(&[json!("%u0041%u0042%u0043")]).unwrap();
        assert_eq!(result, json!("ABC"));

        // Test surrogate pair (emoji)
        let result = global_unescape(&[json!("%uD83D%uDE00")]).unwrap();
        assert_eq!(result, json!("😀"));
    }

    #[test]
    fn test_escape_unescape_roundtrip() {
        let original = "Hello World! @test 123";
        let escaped = global_escape(&[json!(original)]).unwrap();
        let unescaped = global_unescape(&[escaped]).unwrap();
        assert_eq!(unescaped, json!(original));
    }
}
