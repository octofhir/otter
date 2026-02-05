//! Symbol constructor and prototype implementation

use std::sync::{Arc, atomic::{AtomicU64, Ordering}};

use crate::context::NativeContext;
use crate::error::VmError;
use crate::gc::GcRef;
use crate::interpreter::PreferredType;
use crate::intrinsics::well_known;
use crate::memory::MemoryManager;
use crate::object::{JsObject, PropertyAttributes, PropertyDescriptor, PropertyKey};
use crate::string::JsString;
use crate::value::{Symbol, Value};

static NEXT_SYMBOL_ID: AtomicU64 = AtomicU64::new(well_known::UNSCOPABLES + 1);

fn next_symbol_id() -> u64 {
    NEXT_SYMBOL_ID.fetch_add(1, Ordering::Relaxed)
}

fn symbol_from_value(value: &Value) -> Option<GcRef<Symbol>> {
    if let Some(sym) = value.as_symbol() {
        return Some(sym);
    }
    if let Some(obj) = value.as_object() {
        if let Some(prim) = obj
            .get(&PropertyKey::string("__primitiveValue__"))
            .or_else(|| obj.get(&PropertyKey::string("__value__")))
        {
            if let Some(sym) = prim.as_symbol() {
                return Some(sym);
            }
        }
    }
    None
}

fn symbol_to_string(sym: &Symbol) -> String {
    if let Some(desc) = sym.description.as_deref() {
        format!("Symbol({})", desc)
    } else {
        "Symbol()".to_string()
    }
}

fn set_function_properties(func: &Value, name: &str, length: i32, non_constructor: bool) {
    if let Some(obj) = func.as_object() {
        obj.define_property(
            PropertyKey::string("name"),
            PropertyDescriptor::function_length(Value::string(JsString::intern(name))),
        );
        obj.define_property(
            PropertyKey::string("length"),
            PropertyDescriptor::function_length(Value::int32(length)),
        );
        if non_constructor {
            obj.define_property(
                PropertyKey::string("__non_constructor"),
                PropertyDescriptor::builtin_data(Value::boolean(true)),
            );
        }
    }
}

/// Initialize Symbol.prototype with valueOf, toString, @@toPrimitive, and description.
pub fn init_symbol_prototype(
    symbol_proto: GcRef<JsObject>,
    fn_proto: GcRef<JsObject>,
    mm: &Arc<MemoryManager>,
) {
    // Symbol.prototype.valueOf
    let value_of_fn = Value::native_function_with_proto(
        |this_val, _args, _ncx| {
            let sym = symbol_from_value(this_val).ok_or_else(|| {
                VmError::type_error("Symbol.prototype.valueOf requires that 'this' be a Symbol")
            })?;
            Ok(Value::symbol(sym))
        },
        mm.clone(),
        fn_proto.clone(),
    );
    set_function_properties(&value_of_fn, "valueOf", 0, true);
    symbol_proto.define_property(
        PropertyKey::string("valueOf"),
        PropertyDescriptor::builtin_method(value_of_fn),
    );

    // Symbol.prototype.toString
    let to_string_fn = Value::native_function_with_proto(
        |this_val, _args, _ncx| {
            let sym = symbol_from_value(this_val).ok_or_else(|| {
                VmError::type_error("Symbol.prototype.toString requires that 'this' be a Symbol")
            })?;
            let s = symbol_to_string(&sym);
            Ok(Value::string(JsString::intern(&s)))
        },
        mm.clone(),
        fn_proto.clone(),
    );
    set_function_properties(&to_string_fn, "toString", 0, true);
    symbol_proto.define_property(
        PropertyKey::string("toString"),
        PropertyDescriptor::builtin_method(to_string_fn),
    );

    // Symbol.prototype[@@toPrimitive]
    let to_primitive_fn = Value::native_function_with_proto(
        |this_val, _args, _ncx| {
            let sym = symbol_from_value(this_val).ok_or_else(|| {
                VmError::type_error(
                    "Symbol.prototype[@@toPrimitive] requires that 'this' be a Symbol",
                )
            })?;
            Ok(Value::symbol(sym))
        },
        mm.clone(),
        fn_proto.clone(),
    );
    set_function_properties(&to_primitive_fn, "[Symbol.toPrimitive]", 1, true);
    symbol_proto.define_property(
        PropertyKey::Symbol(well_known::TO_PRIMITIVE),
        PropertyDescriptor::builtin_method(to_primitive_fn),
    );

    // Symbol.prototype.description (getter)
    let description_getter = Value::native_function_with_proto(
        |this_val, _args, _ncx| {
            let sym = symbol_from_value(this_val).ok_or_else(|| {
                VmError::type_error("Symbol.prototype.description requires that 'this' be a Symbol")
            })?;
            if let Some(desc) = sym.description.as_deref() {
                Ok(Value::string(JsString::intern(desc)))
            } else {
                Ok(Value::undefined())
            }
        },
        mm.clone(),
        fn_proto.clone(),
    );
    symbol_proto.define_property(
        PropertyKey::string("description"),
        PropertyDescriptor::Accessor {
            get: Some(description_getter),
            set: None,
            attributes: PropertyAttributes::builtin_method(),
        },
    );

    // Symbol.prototype[Symbol.toStringTag] = "Symbol"
    symbol_proto.define_property(
        PropertyKey::Symbol(well_known::TO_STRING_TAG),
        PropertyDescriptor::builtin_data(Value::string(JsString::intern("Symbol"))),
    );
}

/// Install static methods on the Symbol constructor.
pub fn install_symbol_statics(
    symbol_ctor: GcRef<JsObject>,
    fn_proto: GcRef<JsObject>,
    mm: &Arc<MemoryManager>,
) {
    // Symbol.for(key)
    let for_fn = Value::native_function_with_proto(
        |_this, args, ncx| {
            let key_val = args.first().cloned().unwrap_or(Value::undefined());
            let key = ncx.to_string_value(&key_val)?;
            let registry = ncx.ctx.symbol_registry().clone();
            if let Some(sym) = registry.get(&key) {
                return Ok(Value::symbol(sym));
            }
            let sym = GcRef::new(Symbol {
                description: Some(key.clone()),
                id: next_symbol_id(),
            });
            registry.insert(key, sym);
            Ok(Value::symbol(sym))
        },
        mm.clone(),
        fn_proto.clone(),
    );
    set_function_properties(&for_fn, "for", 1, true);
    symbol_ctor.define_property(
        PropertyKey::string("for"),
        PropertyDescriptor::builtin_method(for_fn),
    );

    // Symbol.keyFor(sym)
    let key_for_fn = Value::native_function_with_proto(
        |_this, args, ncx| {
            let sym_val = args.first().cloned().unwrap_or(Value::undefined());
            let sym = sym_val.as_symbol().ok_or_else(|| {
                VmError::type_error("Symbol.keyFor requires that the argument be a Symbol")
            })?;
            let registry = ncx.ctx.symbol_registry().clone();
            if let Some(key) = registry.key_for(&sym) {
                return Ok(Value::string(JsString::intern(&key)));
            }
            Ok(Value::undefined())
        },
        mm.clone(),
        fn_proto,
    );
    set_function_properties(&key_for_fn, "keyFor", 1, true);
    symbol_ctor.define_property(
        PropertyKey::string("keyFor"),
        PropertyDescriptor::builtin_method(key_for_fn),
    );

    // Symbol.length = 0 and Symbol is not a constructor
    let ctor_val = Value::object(symbol_ctor);
    set_function_properties(&ctor_val, "Symbol", 0, true);
}

/// Create Symbol constructor function (callable, not constructable).
pub fn create_symbol_constructor(
) -> Box<dyn Fn(&Value, &[Value], &mut NativeContext<'_>) -> Result<Value, VmError> + Send + Sync>
{
    Box::new(|_this, args, ncx| {
        if ncx.is_construct() {
            return Err(VmError::type_error("Symbol is not a constructor"));
        }

        let desc = if let Some(arg) = args.first() {
            let prim = ncx.to_primitive(arg, PreferredType::String)?;
            if prim.is_undefined() {
                None
            } else if prim.is_symbol() {
                return Err(VmError::type_error(
                    "Cannot convert a Symbol value to a string",
                ));
            } else {
                Some(ncx.to_string_value(&prim)?)
            }
        } else {
            None
        };

        let sym = Symbol {
            description: desc,
            id: next_symbol_id(),
        };
        Ok(Value::symbol(GcRef::new(sym)))
    })
}
