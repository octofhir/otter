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
fn destructuring_assignment_supports_array_and_object_patterns() {
    let result = execute_test262_basic(
        concat!(
            "var first, second, inner, tail;\n",
            "var returned = ([first, second = function() {}, ...tail] = [1, undefined, 3, 4]);\n",
            "assert.sameValue(first, 1, 'array destructuring assignment stores first element');\n",
            "assert.sameValue(second.name, 'second', 'array destructuring assignment default infers identifier name');\n",
            "assert.sameValue(tail.length, 2, 'array destructuring assignment materializes rest array');\n",
            "assert.sameValue(tail[0], 3, 'array destructuring assignment rest keeps trailing element 0');\n",
            "assert.sameValue(tail[1], 4, 'array destructuring assignment rest keeps trailing element 1');\n",
            "assert.sameValue(returned[0], 1, 'destructuring assignment expression returns original rhs value');\n",
            "var renamed, rest;\n",
            "({ value: renamed = 7, nested: { inner }, ...rest } = { value: undefined, nested: { inner: 9 }, keep: 11 });\n",
            "assert.sameValue(renamed, 7, 'object destructuring assignment applies default');\n",
            "assert.sameValue(inner, 9, 'object destructuring assignment supports nested patterns');\n",
            "assert.sameValue(rest.keep, 11, 'object destructuring assignment supports object rest');\n",
            "assert.sameValue(rest.value, undefined, 'object destructuring assignment excludes extracted key');\n",
        ),
        "native-test262-destructuring-assignment-patterns.js",
    );

    assert_eq!(result, RegisterValue::from_i32(0));
}

#[test]
fn destructuring_assignment_supports_member_targets() {
    let result = execute_test262_basic(
        concat!(
            "var holder = { left: 0, nested: { right: 0 } };\n",
            "([holder.left, holder.nested.right] = [5, 8]);\n",
            "assert.sameValue(holder.left, 5, 'array destructuring assignment stores into static member target');\n",
            "assert.sameValue(holder.nested.right, 8, 'array destructuring assignment stores into nested member target');\n",
            "var key = 'value';\n",
            "var target = {};\n",
            "({ [key]: target[key] } = { value: 13 });\n",
            "assert.sameValue(target.value, 13, 'object destructuring assignment stores into computed member target');\n",
        ),
        "native-test262-destructuring-assignment-members.js",
    );

    assert_eq!(result, RegisterValue::from_i32(0));
}
