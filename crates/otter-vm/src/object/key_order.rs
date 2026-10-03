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

/// The array index `key` names (§6.1.7), or `None` for any other string.
pub(crate) fn array_index_property_name(key: &str) -> Option<u32> {
    array_index_property_bytes(key.as_bytes())
}

/// [`array_index_property_name`] over Latin-1 code units: the canonical
/// decimal spelling of an integer below `2^32 - 1`. Allocation-free, so key
/// enumeration can classify a shape's own key strings in place.
pub(crate) fn array_index_property_bytes(key: &[u8]) -> Option<u32> {
    let (&first, rest) = key.split_first()?;
    if !first.is_ascii_digit() || (first == b'0' && !rest.is_empty()) || key.len() > 10 {
        return None;
    }
    let mut value = u64::from(first - b'0');
    for &unit in rest {
        if !unit.is_ascii_digit() {
            return None;
        }
        value = value * 10 + u64::from(unit - b'0');
    }
    u32::try_from(value).ok().filter(|&index| index != u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::{array_index_property_bytes, array_index_property_name};

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
        assert_eq!(array_index_property_name("+1"), None);
        assert_eq!(array_index_property_name("99999999999"), None);
        assert_eq!(array_index_property_bytes(b"42"), Some(42));
    }
}
