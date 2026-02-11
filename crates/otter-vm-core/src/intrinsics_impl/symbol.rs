//! Symbol constructor and prototype implementation

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use crate::context::NativeContext;
use crate::error::VmError;
use crate::gc::GcRef;
use crate::interpreter::PreferredType;
use crate::intrinsics::well_known;
use crate::memory::MemoryManager;
use crate::object::{JsObject, PropertyAttributes, PropertyDescriptor, PropertyKey};
use crate::string::JsString;
use crate::value::{Symbol, Value};
use otter_macros::dive;

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

#[dive(name = "valueOf", length = 0)]
fn symbol_value_of(
    this_val: &Value,
    _args: &[Value],
    _ncx: &mut NativeContext<'_>,
) -> Result<Value, VmError> {
    let sym = symbol_from_value(this_val).ok_or_else(|| {
        VmError::type_error("Symbol.prototype.valueOf requires that 'this' be a Symbol")
    })?;
    Ok(Value::symbol(sym))
}

#[dive(name = "toString", length = 0)]
fn symbol_to_string_fn(
    this_val: &Value,
    _args: &[Value],
    _ncx: &mut NativeContext<'_>,
) -> Result<Value, VmError> {
    let sym = symbol_from_value(this_val).ok_or_else(|| {
        VmError::type_error("Symbol.prototype.toString requires that 'this' be a Symbol")
    })?;
    let s = symbol_to_string(&sym);
    Ok(Value::string(JsString::intern(&s)))
}

#[dive(name = "[Symbol.toPrimitive]", length = 1)]
fn symbol_to_primitive_fn(
    this_val: &Value,
    _args: &[Value],
    _ncx: &mut NativeContext<'_>,
) -> Result<Value, VmError> {
    let sym = symbol_from_value(this_val).ok_or_else(|| {
        VmError::type_error("Symbol.prototype[@@toPrimitive] requires that 'this' be a Symbol")
    })?;
    Ok(Value::symbol(sym))
}

#[dive(name = "get description", length = 0)]
fn symbol_description_getter(
    this_val: &Value,
    _args: &[Value],
    _ncx: &mut NativeContext<'_>,
) -> Result<Value, VmError> {
    let sym = symbol_from_value(this_val).ok_or_else(|| {
        VmError::type_error("Symbol.prototype.description requires that 'this' be a Symbol")
    })?;
    if let Some(desc) = sym.description.as_deref() {
        Ok(Value::string(JsString::intern(desc)))
    } else {
        Ok(Value::undefined())
    }
}

#[dive(name = "for", length = 1)]
fn symbol_for_static(
    _this: &Value,
    args: &[Value],
    ncx: &mut NativeContext<'_>,
) -> Result<Value, VmError> {
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
}

#[dive(name = "keyFor", length = 1)]
fn symbol_key_for_static(
    _this: &Value,
    args: &[Value],
    ncx: &mut NativeContext<'_>,
) -> Result<Value, VmError> {
    let sym_val = args.first().cloned().unwrap_or(Value::undefined());
    let sym = sym_val.as_symbol().ok_or_else(|| {
        VmError::type_error("Symbol.keyFor requires that the argument be a Symbol")
    })?;
    let registry = ncx.ctx.symbol_registry().clone();
    if let Some(key) = registry.key_for(&sym) {
        return Ok(Value::string(JsString::intern(&key)));
    }
    Ok(Value::undefined())
}

/// Initialize Symbol.prototype with valueOf, toString, @@toPrimitive, and description.
pub fn init_symbol_prototype(
    symbol_proto: GcRef<JsObject>,
    _fn_proto: GcRef<JsObject>,
    mm: &Arc<MemoryManager>,
) {
    let methods: &[(&str, crate::value::NativeFn, u32)] =
        &[symbol_value_of_decl(), symbol_to_string_fn_decl()];
    for (name, native_fn, length) in methods {
        let fn_val = Value::native_function_from_decl(name, native_fn.clone(), *length, mm.clone());
        symbol_proto.define_property(
            PropertyKey::string(name),
            PropertyDescriptor::builtin_method(fn_val),
        );
    }

    // Symbol.prototype[@@toPrimitive]
    let (to_primitive_name, to_primitive_native, to_primitive_length) =
        symbol_to_primitive_fn_decl();
    let to_primitive_fn = Value::native_function_from_decl(
        to_primitive_name,
        to_primitive_native,
        to_primitive_length,
        mm.clone(),
    );
    symbol_proto.define_property(
        PropertyKey::Symbol(well_known::to_primitive_symbol()),
        PropertyDescriptor::data_with_attrs(to_primitive_fn, PropertyAttributes::function_length()),
    );

    // Symbol.prototype.description (getter)
    let (desc_name, desc_native, desc_length) = symbol_description_getter_decl();
    let description_getter =
        Value::native_function_from_decl(desc_name, desc_native, desc_length, mm.clone());
    if let Some(desc_obj) = description_getter.as_object() {
        desc_obj.define_property(
            PropertyKey::string("name"),
            PropertyDescriptor::function_length(Value::string(JsString::intern("get description"))),
        );
    }
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
        PropertyKey::Symbol(well_known::to_string_tag_symbol()),
        PropertyDescriptor::data_with_attrs(
            Value::string(JsString::intern("Symbol")),
            PropertyAttributes::function_length(),
        ),
    );
}

/// Install static methods on the Symbol constructor.
pub fn install_symbol_statics(
    symbol_ctor: GcRef<JsObject>,
    _fn_proto: GcRef<JsObject>,
    mm: &Arc<MemoryManager>,
) {
    let methods: &[(&str, crate::value::NativeFn, u32)] =
        &[symbol_for_static_decl(), symbol_key_for_static_decl()];
    for (name, native_fn, length) in methods {
        let fn_val = Value::native_function_from_decl(name, native_fn.clone(), *length, mm.clone());
        symbol_ctor.define_property(
            PropertyKey::string(name),
            PropertyDescriptor::builtin_method(fn_val),
        );
    }

    // Symbol.length = 0 (Symbol has [[Construct]] but throws on construction)
    let ctor_val = Value::object(symbol_ctor);
    set_function_properties(&ctor_val, "Symbol", 0, false);
}

/// Create Symbol constructor function (callable, not constructable).
pub fn create_symbol_constructor()
-> Box<dyn Fn(&Value, &[Value], &mut NativeContext<'_>) -> Result<Value, VmError> + Send + Sync> {
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
