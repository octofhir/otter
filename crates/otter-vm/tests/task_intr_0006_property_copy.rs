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

#[test]
fn object_rest_rechecks_enumerability_after_earlier_get_side_effects() {
    let result = execute_test262_basic(
        concat!(
            "var source = {};\n",
            "Object.defineProperty(source, 'first', {\n",
            "  enumerable: true,\n",
            "  configurable: true,\n",
            "  get: function () {\n",
            "    Object.defineProperty(source, 'second', {\n",
            "      value: 2,\n",
            "      writable: true,\n",
            "      enumerable: false,\n",
            "      configurable: true\n",
            "    });\n",
            "    return 1;\n",
            "  }\n",
            "});\n",
            "source.second = 2;\n",
            "function capture({ ...rest }) { return rest; }\n",
            "var rest = capture(source);\n",
            "assert.sameValue(rest.first, 1, 'object rest copies enumerable getter-backed property');\n",
            "assert.sameValue(rest.second, undefined, 'object rest rechecks descriptor enumerable flag during copy');\n",
            "var desc = Object.getOwnPropertyDescriptor(rest, 'first');\n",
            "assert.sameValue(typeof desc.get, 'undefined', 'object rest creates data property instead of accessor');\n",
            "assert.sameValue(desc.value, 1, 'object rest stores getter result as data value');\n",
        ),
        "native-test262-object-rest-rechecks-enumerability.js",
    );

    assert_eq!(result, RegisterValue::from_i32(0));
}
