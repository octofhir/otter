use crate::object::{JsObject, PropertyAttributes, PropertyDescriptor, PropertyKey};
use crate::string::JsString;
use crate::value::Value;
use regress::{Flags, Regex};
use std::sync::Arc;

/// JavaScript RegExp object
#[derive(Debug)]
pub struct JsRegExp {
    /// The Ordinary Object part (properties like lastIndex)
    pub object: Arc<JsObject>,
    /// The regex pattern
    pub pattern: String,
    /// The regex flags
    pub flags: String,
    /// Whether this regex uses Unicode (u or v flags)
    pub unicode: bool,
    /// The compiled Rust regex (if compilation succeeded)
    pub native_regex: Option<Regex>,
}

impl JsRegExp {
    /// Create a new JsRegExp
    pub fn new(pattern: String, flags: String, proto: Option<Arc<JsObject>>) -> Self {
        let object = Arc::new(JsObject::new(proto));
        let source = if pattern.is_empty() {
            "(?:)".to_string()
        } else {
            pattern.clone()
        };
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
        object.define_property(
            PropertyKey::string("source"),
            PropertyDescriptor::data_with_attrs(
                Value::string(JsString::intern(&source)),
                PropertyAttributes {
                    writable: false,
                    enumerable: false,
                    configurable: false,
                },
            ),
        );
        object.define_property(
            PropertyKey::string("flags"),
            PropertyDescriptor::data_with_attrs(
                Value::string(JsString::intern(&flags)),
                PropertyAttributes {
                    writable: false,
                    enumerable: false,
                    configurable: false,
                },
            ),
        );
        let flag_attrs = PropertyAttributes {
            writable: false,
            enumerable: false,
            configurable: true,
        };
        let flag_prop = |name: &str, enabled: bool| {
            object.define_property(
                PropertyKey::string(name),
                PropertyDescriptor::data_with_attrs(Value::boolean(enabled), flag_attrs),
            );
        };
        flag_prop("global", flags.contains('g'));
        flag_prop("ignoreCase", flags.contains('i'));
        flag_prop("multiline", flags.contains('m'));
        flag_prop("dotAll", flags.contains('s'));
        flag_prop("sticky", flags.contains('y'));
        flag_prop("unicode", flags.contains('u'));
        flag_prop("unicodeSets", flags.contains('v'));
        flag_prop("hasIndices", flags.contains('d'));
        let parsed_flags = Flags::from(flags.as_str());
        let unicode = parsed_flags.unicode || parsed_flags.unicode_sets;
        let native_regex = Regex::with_flags(&pattern, parsed_flags).ok();

        Self {
            object,
            pattern,
            flags,
            unicode,
            native_regex,
        }
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
