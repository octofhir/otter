use crate::context::NativeContext;
use crate::error::VmError;
use crate::gc::GcRef;
use crate::memory::MemoryManager;
use crate::object::{JsObject, PropertyAttributes, PropertyDescriptor, PropertyKey};
use crate::string::JsString;
use crate::value::Value;
use std::sync::Arc;

use super::common::*;

/// Install Instant constructor and prototype onto `temporal_obj`.
pub(super) fn install_instant(
    temporal_obj: &GcRef<JsObject>,
    obj_proto: &GcRef<JsObject>,
    fn_proto: &GcRef<JsObject>,
    mm: &Arc<MemoryManager>,
) {
    let proto = GcRef::new(JsObject::new(Value::object(obj_proto.clone()), mm.clone()));
    let ctor_obj = GcRef::new(JsObject::new(Value::object(fn_proto.clone()), mm.clone()));

    ctor_obj.define_property(
        PropertyKey::string("prototype"),
        PropertyDescriptor::data_with_attrs(
            Value::object(proto.clone()),
            PropertyAttributes { writable: false, enumerable: false, configurable: false },
        ),
    );
    ctor_obj.define_property(
        PropertyKey::string("name"),
        PropertyDescriptor::function_length(Value::string(JsString::intern("Instant"))),
    );
    ctor_obj.define_property(
        PropertyKey::string("length"),
        PropertyDescriptor::function_length(Value::number(0.0)),
    );

    // Constructor: stores SLOT_TEMPORAL_TYPE + epochNanoseconds
    let ctor_fn: Box<
        dyn Fn(&Value, &[Value], &mut NativeContext<'_>) -> Result<Value, VmError> + Send + Sync,
    > = Box::new(|this, args, _ncx| {
        if let Some(obj) = this.as_object() {
            obj.define_property(
                PropertyKey::string(SLOT_TEMPORAL_TYPE),
                PropertyDescriptor::builtin_data(Value::string(JsString::intern("Instant"))),
            );
            if let Some(epoch_ns) = args.first() {
                obj.define_property(
                    PropertyKey::string("epochNanoseconds"),
                    PropertyDescriptor::builtin_data(epoch_ns.clone()),
                );
            }
        }
        Ok(Value::undefined())
    });

    let ctor_value = Value::native_function_with_proto_and_object(
        Arc::from(ctor_fn),
        mm.clone(),
        fn_proto.clone(),
        ctor_obj.clone(),
    );

    // prototype.constructor
    proto.define_property(
        PropertyKey::string("constructor"),
        PropertyDescriptor::data_with_attrs(ctor_value.clone(), PropertyAttributes::constructor_link()),
    );

    // Instant.from() — pass through to constructor
    let from_ctor = ctor_value.clone();
    let from_fn = Value::native_function_with_proto_named(
        move |_this, args, ncx| {
            let item = args.first().cloned().unwrap_or(Value::undefined());
            ncx.call_function_construct(&from_ctor, Value::undefined(), &[item])
        },
        mm.clone(),
        fn_proto.clone(),
        "from",
        1,
    );
    ctor_obj.define_property(
        PropertyKey::string("from"),
        PropertyDescriptor::data_with_attrs(from_fn, PropertyAttributes::builtin_method()),
    );

    // Register on namespace
    temporal_obj.define_property(
        PropertyKey::string("Instant"),
        PropertyDescriptor::data_with_attrs(ctor_value, PropertyAttributes::builtin_method()),
    );
}
