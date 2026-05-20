//! Ordinary own-property key ordering.
//!
//! ECMA-262 exposes own string keys in a deterministic order: array-index
//! property names first in ascending numeric order, then every other string key
//! in insertion order. Symbols are stored separately by `object.rs` and are
//! appended by callers that need full `[[OwnPropertyKeys]]`.
//!
//! # Invariants
//! - Non-index strings keep the exact insertion order encoded by the object
//!   shape/dictionary key table.
//! - `"4294967295"` is not an array index property name.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-ordinaryownpropertykeys>
//! - <https://tc39.es/ecma262/#array-index>

pub(crate) fn array_index_property_name(key: &str) -> Option<u32> {
    if key.is_empty() {
        return None;
    }
    if key.len() > 1 && key.as_bytes().first() == Some(&b'0') {
        return None;
    }
    let value = key.parse::<u32>().ok()?;
    if value == u32::MAX {
        return None;
    }
    if value.to_string() == key {
        Some(value)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::array_index_property_name;

    #[test]
    fn recognises_array_index_property_names() {
        assert_eq!(array_index_property_name("0"), Some(0));
        assert_eq!(array_index_property_name("10"), Some(10));
        assert_eq!(array_index_property_name("4294967294"), Some(4_294_967_294));

        assert_eq!(array_index_property_name(""), None);
        assert_eq!(array_index_property_name("01"), None);
        assert_eq!(array_index_property_name("-1"), None);
        assert_eq!(array_index_property_name("1.0"), None);
        assert_eq!(array_index_property_name("4294967295"), None);
    }
}
