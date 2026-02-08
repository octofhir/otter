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
    /// The compiled Rust regex (if compilation succeeded)
    pub native_regex: Option<Regex>,
}

impl otter_vm_gc::GcTraceable for JsRegExp {
    const NEEDS_TRACE: bool = true;
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
        let native_regex = Regex::with_flags(&pattern, parsed_flags).ok();

        Self {
            object,
            pattern,
            flags,
            unicode,
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
