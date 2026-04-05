//! String-literal side tables for the new VM.
//!
//! §6.1.4 — String literals are stored as WTF-16 `JsString` values to
//! correctly preserve lone surrogates from source code.
//!
//! Spec: <https://tc39.es/ecma262/#sec-ecmascript-language-types-string-type>

use crate::js_string::JsString;

/// Stable string-literal identifier inside a function side table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StringId(pub u16);

/// Immutable string-literal table for a function.
///
/// Stores WTF-16 `JsString` values to preserve lone surrogates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StringTable {
    values: Box<[JsString]>,
}

impl StringTable {
    /// Creates a string-literal table from UTF-8 values.
    ///
    /// For strings that need lone surrogate support, use [`new_js`].
    #[must_use]
    pub fn new(values: Vec<impl Into<Box<str>>>) -> Self {
        let values = values
            .into_iter()
            .map(|v| JsString::from_str(&v.into()))
            .collect::<Vec<JsString>>()
            .into_boxed_slice();
        Self { values }
    }

    /// Creates a string-literal table from `JsString` values directly.
    #[must_use]
    pub fn new_js(values: Vec<JsString>) -> Self {
        Self {
            values: values.into_boxed_slice(),
        }
    }

    /// Creates an empty string-literal table.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            values: Box::new([]),
        }
    }

    /// Returns the number of string literals.
    #[must_use]
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// Returns `true` when the table is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// Returns the `JsString` for the given identifier.
    #[must_use]
    pub fn get_js(&self, id: StringId) -> Option<&JsString> {
        self.values.get(usize::from(id.0))
    }

    /// Returns the string literal as a UTF-8 `&str` (lossy for lone surrogates).
    ///
    /// This allocates a temporary `String` — prefer `get_js` for new code.
    #[must_use]
    pub fn get(&self, id: StringId) -> Option<String> {
        self.values
            .get(usize::from(id.0))
            .map(|js| js.to_rust_string())
    }
}

impl Default for StringTable {
    fn default() -> Self {
        Self::empty()
    }
}

#[cfg(test)]
mod tests {
    use super::{StringId, StringTable};

    #[test]
    fn string_table_resolves_literals() {
        let table = StringTable::new(vec!["otter", "vm"]);

        assert_eq!(table.len(), 2);
        assert_eq!(table.get(StringId(0)), Some("otter".to_string()));
        assert_eq!(table.get(StringId(1)), Some("vm".to_string()));
        assert_eq!(table.get(StringId(2)), None);
    }

    #[test]
    fn string_table_preserves_utf16() {
        use crate::js_string::JsString;
        let lone = JsString::from_utf16(vec![0xD800]);
        let table = StringTable::new_js(vec![lone.clone()]);
        let retrieved = table.get_js(StringId(0)).unwrap();
        assert_eq!(retrieved.as_utf16(), &[0xD800]);
    }
}
