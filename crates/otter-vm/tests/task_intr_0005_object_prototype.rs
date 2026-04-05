use otter_vm::interpreter::InterpreterError;
use otter_vm::object::ObjectHeap;
use otter_vm::property::PropertyNameId;
use otter_vm::source::compile_test262_basic_script;
use otter_vm::value::RegisterValue;
use otter_vm::{Interpreter, RuntimeState};

fn execute_test262_basic(source: &str, source_url: &str) -> RegisterValue {
    let module = compile_test262_basic_script(source, source_url)
        .expect("test262 basic script should compile on the new VM path");

    let mut runtime = RuntimeState::new();
    let global = runtime.intrinsics().global_object();
    let registers = [RegisterValue::from_object_handle(global.0)];
    Interpreter::new()
        .execute_with_runtime(
            &module,
            otter_vm::module::FunctionIndex(0),
            &registers,
            &mut runtime,
        )
        .expect("test262 basic script should execute on the new VM path")
        .return_value()
}

fn execute_test262_basic_without_throw(source: &str, source_url: &str) -> Result<(), String> {
    let module = compile_test262_basic_script(source, source_url)
        .expect("test262 basic script should compile on the new VM path");

    let mut runtime = RuntimeState::new();
    let global = runtime.intrinsics().global_object();
    let registers = [RegisterValue::from_object_handle(global.0)];
    match Interpreter::new().execute_with_runtime(
        &module,
        otter_vm::module::FunctionIndex(0),
        &registers,
        &mut runtime,
    ) {
        Ok(_) => Ok(()),
        Err(InterpreterError::UncaughtThrow(value)) => {
            let Some(handle) = value.as_object_handle().map(otter_vm::object::ObjectHandle) else {
                return Err(format!("non-string throw: {:?}", value));
            };
            let message = runtime
                .objects()
                .string_value(handle)
                .expect("thrown string lookup should succeed")
                .map(|s| s.to_rust_string())
                .unwrap_or_else(|| "<non-string>".to_string());
            Err(message)
        }
        Err(error) => Err(error.to_string()),
    }
}

fn strip_test262_frontmatter(source: &str) -> &str {
    source
        .find("---*/")
        .map(|index| &source[index + 5..])
        .unwrap_or(source)
}

#[test]
fn object_heap_delete_property_removes_named_slot() {
    let mut heap = ObjectHeap::new();
    let object = heap.alloc_object();
    let property = PropertyNameId(7);

    heap.set_property(object, property, RegisterValue::from_i32(42))
        .expect("property store should succeed");
    assert!(
        heap.delete_property(object, property)
            .expect("delete should succeed")
    );
    assert_eq!(
        heap.get_property(object, property)
            .expect("post-delete lookup should succeed"),
        None
    );
}

#[test]
fn delete_expression_returns_true_for_existing_and_missing_named_properties() {
    let result = execute_test262_basic(
        concat!(
            "var object = { present: 1 };\n",
            "assert.sameValue(delete object.present, true, \"delete existing property\");\n",
            "assert.sameValue(object.present, undefined, \"deleted property is absent\");\n",
            "assert.sameValue(delete object.missing, true, \"delete missing property\");\n",
        ),
        "native-test262-delete-named-property.js",
    );

    assert_eq!(result, RegisterValue::from_i32(0));
}

#[test]
fn official_string_constructor_slice_passes_through_object_prototype_fallback() {
    let source = include_str!(
        "../../../tests/test262/test/built-ins/String/prototype/constructor/S15.5.4.1_A1_T2.js"
    );

    let result = execute_test262_basic(strip_test262_frontmatter(source), "S15.5.4.1_A1_T2.js");
    assert_eq!(result, RegisterValue::from_i32(0));
}

#[test]
fn string_wrapper_loose_inequality_matches_primitive_value() {
    let result = execute_test262_basic(
        concat!(
            "var constr = String.prototype.constructor;\n",
            "var instance = new constr(\"choosing one\");\n",
            "if (instance != \"choosing one\") {\n",
            "  throw new Test262Error(\"loose equality failed\");\n",
            "}\n",
        ),
        "native-test262-string-wrapper-loose-inequality.js",
    );

    assert_eq!(result, RegisterValue::from_i32(0));
}

#[test]
fn string_wrapper_constructor_link_survives_constructor_indirection() {
    let result = execute_test262_basic(
        concat!(
            "var constr = String.prototype.constructor;\n",
            "var instance = new constr(\"choosing one\");\n",
            "if (instance.constructor !== String) {\n",
            "  throw new Test262Error(\"constructor link failed\");\n",
            "}\n",
        ),
        "native-test262-string-wrapper-constructor-link.js",
    );

    assert_eq!(result, RegisterValue::from_i32(0));
}

#[test]
fn combined_string_constructor_slice_conditions_pass_together() {
    let outcome = execute_test262_basic_without_throw(
        concat!(
            "var __constr = String.prototype.constructor;\n",
            "var __instance = new __constr(\"choosing one\");\n",
            "if (__instance != \"choosing one\") {\n",
            "  throw \"check0\";\n",
            "}\n",
            "if (__instance.constructor !== String) {\n",
            "  throw \"check1\";\n",
            "}\n",
            "if (!(String.prototype.isPrototypeOf(__instance))) {\n",
            "  throw \"check2\";\n",
            "}\n",
            "var __to_string_result = '[object ' + 'String' + ']';\n",
            "delete String.prototype.toString;\n",
            "if (__instance.toString() !== __to_string_result) {\n",
            "  throw __instance.toString() + \"|\" + __to_string_result;\n",
            "}\n",
        ),
        "native-test262-string-constructor-slice-combined.js",
    );

    assert_eq!(outcome, Ok(()));
}

#[test]
fn delete_removes_bootstrap_method_and_exposes_object_prototype_fallback() {
    let result = execute_test262_basic(
        concat!(
            "var instance = new String(\"otter\");\n",
            "assert.sameValue(delete String.prototype.toString, true, \"delete String.prototype.toString\");\n",
            "assert.sameValue(String.prototype.isPrototypeOf(instance), true, \"String.prototype in chain\");\n",
            "assert.sameValue(instance.toString(), \"[object String]\", \"Object.prototype.toString fallback\");\n",
        ),
        "native-test262-object-prototype-fallback.js",
    );

    assert_eq!(result, RegisterValue::from_i32(0));
}
