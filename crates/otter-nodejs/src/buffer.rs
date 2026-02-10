//! Buffer module - Node.js-compatible binary data handling
//!
//! Provides the Buffer class wrapping Uint8Array with encoding support.

use otter_vm_runtime::extension::{Op, op_sync};
use serde_json::{Value as JsonValue, json};

/// Create Buffer native operations
pub fn buffer_ops() -> Vec<Op> {
    vec![
        op_sync("__buffer_alloc", buffer_alloc),
        op_sync("__buffer_from_string", buffer_from_string),
        op_sync("__buffer_to_string", buffer_to_string),
        op_sync("__buffer_byte_length", buffer_byte_length),
    ]
}

/// Allocate a new buffer of given size, optionally filled with a value
fn buffer_alloc(args: &[JsonValue]) -> Result<JsonValue, String> {
    let size = args
        .first()
        .and_then(|v| v.as_u64())
        .ok_or("Buffer.alloc requires size argument")?;

    let fill = args.get(1).and_then(|v| v.as_u64()).unwrap_or(0) as u8;

    // Return array of bytes
    let bytes: Vec<u8> = vec![fill; size as usize];
    Ok(json!(bytes))
}

/// Create buffer from string with encoding
fn buffer_from_string(args: &[JsonValue]) -> Result<JsonValue, String> {
    let string = args
        .first()
        .and_then(|v| v.as_str())
        .ok_or("Buffer.from requires string argument")?;

    let encoding = args.get(1).and_then(|v| v.as_str()).unwrap_or("utf8");

    let bytes = match encoding {
        "utf8" | "utf-8" => string.as_bytes().to_vec(),
        "hex" => decode_hex(string)?,
        "base64" => base64::Engine::decode(&base64::engine::general_purpose::STANDARD, string)
            .map_err(|e| format!("Invalid base64: {}", e))?,
        "latin1" | "binary" => string.bytes().collect(),
        "ascii" => string.bytes().map(|b| b & 0x7f).collect(),
        _ => return Err(format!("Unknown encoding: {}", encoding)),
    };

    Ok(json!(bytes))
}

/// Convert buffer to string with encoding
fn buffer_to_string(args: &[JsonValue]) -> Result<JsonValue, String> {
    let bytes: Vec<u8> = args
        .first()
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_u64().map(|n| n as u8))
                .collect()
        })
        .ok_or("Buffer.toString requires byte array")?;

    let encoding = args.get(1).and_then(|v| v.as_str()).unwrap_or("utf8");

    let result = match encoding {
        "utf8" | "utf-8" => String::from_utf8_lossy(&bytes).to_string(),
        "hex" => encode_hex(&bytes),
        "base64" => base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &bytes),
        "latin1" | "binary" => bytes.iter().map(|&b| b as char).collect(),
        "ascii" => bytes.iter().map(|&b| (b & 0x7f) as char).collect(),
        _ => return Err(format!("Unknown encoding: {}", encoding)),
    };

    Ok(json!(result))
}

/// Get byte length of string in given encoding
fn buffer_byte_length(args: &[JsonValue]) -> Result<JsonValue, String> {
    let string = args
        .first()
        .and_then(|v| v.as_str())
        .ok_or("Buffer.byteLength requires string argument")?;

    let encoding = args.get(1).and_then(|v| v.as_str()).unwrap_or("utf8");

    let len = match encoding {
        "utf8" | "utf-8" => string.len(),
        "hex" => string.len() / 2,
        "base64" => (string.len() * 3) / 4, // Approximate
        "latin1" | "binary" | "ascii" => string.len(),
        _ => return Err(format!("Unknown encoding: {}", encoding)),
    };

    Ok(json!(len))
}

/// Decode hex string to bytes
fn decode_hex(hex: &str) -> Result<Vec<u8>, String> {
    if hex.len() % 2 != 0 {
        return Err("Invalid hex string length".to_string());
    }

    (0..hex.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&hex[i..i + 2], 16)
                .map_err(|_| format!("Invalid hex character at position {}", i))
        })
        .collect()
}

/// Encode bytes to hex string
fn encode_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_buffer_alloc() {
        let result = buffer_alloc(&[json!(5)]).unwrap();
        assert_eq!(result, json!([0, 0, 0, 0, 0]));

        let result = buffer_alloc(&[json!(3), json!(0xff)]).unwrap();
        assert_eq!(result, json!([255, 255, 255]));
    }

    #[test]
    fn test_buffer_from_string_utf8() {
        let result = buffer_from_string(&[json!("hello")]).unwrap();
        assert_eq!(result, json!([104, 101, 108, 108, 111]));
    }

    #[test]
    fn test_buffer_from_string_hex() {
        let result = buffer_from_string(&[json!("48656c6c6f"), json!("hex")]).unwrap();
        assert_eq!(result, json!([72, 101, 108, 108, 111])); // "Hello"
    }

    #[test]
    fn test_buffer_to_string_utf8() {
        let result = buffer_to_string(&[json!([104, 101, 108, 108, 111])]).unwrap();
        assert_eq!(result, json!("hello"));
    }

    #[test]
    fn test_buffer_to_string_hex() {
        let result = buffer_to_string(&[json!([72, 101, 108, 108, 111]), json!("hex")]).unwrap();
        assert_eq!(result, json!("48656c6c6f"));
    }
}
