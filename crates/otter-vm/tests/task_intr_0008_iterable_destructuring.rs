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
fn array_destructuring_uses_iterator_semantics_for_assignment_and_parameters() {
    let result = execute_test262_basic(
        concat!(
            "var first, rest;\n",
            "[first, ...rest] = 'ot';\n",
            "assert.sameValue(first, 'o', 'array destructuring assignment reads string via iterator');\n",
            "assert.sameValue(rest.length, 1, 'array destructuring assignment rest exhausts iterator');\n",
            "assert.sameValue(rest[0], 't', 'array destructuring assignment rest keeps trailing iterator value');\n",
            "function read([a, b]) { return a + b; }\n",
            "assert.sameValue(read('js'), 'js', 'array destructuring parameters read iterable strings');\n",
            "var threw = false;\n",
            "try {\n",
            "  [first] = { 0: 1, length: 1 };\n",
            "} catch (error) {\n",
            "  threw = error instanceof TypeError;\n",
            "}\n",
            "assert.sameValue(threw, true, 'array destructuring assignment rejects non-iterable array-like objects');\n",
        ),
        "native-test262-iterable-array-destructuring.js",
    );

    assert_eq!(result, RegisterValue::from_i32(0));
}
