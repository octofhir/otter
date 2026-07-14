//! Nested compiled try/catch regression: a compiled function's own `catch`
//! must handle a throw from a compiled callee, even when the function is itself
//! called from within another frame's active `try` (compiled -> compiled ->
//! compiled). The innermost handler must win, matching the interpreter.

use otter_runtime::{JitSelection, Runtime, SourceInput};

const SOURCE: &str = r#"
function inner_catches(fn) {
  try { fn(); } catch (e) { return "INNER"; }
  return "no";
}
function outer(cb) {
  var r = "";
  for (var i = 0; i < 300; i++) {
    try { r = cb(); } catch (e) { r = "OUTER"; }
  }
  return r;
}
outer(function () {
  return inner_catches(function () { throw new TypeError(); });
});
"#;

fn run(sel: JitSelection) -> String {
    let mut rt = Runtime::builder().jit_selection(sel).build().expect("rt");
    rt.run_script(SourceInput::from_javascript(SOURCE.to_string()), "nested.js")
        .expect("run")
        .completion_string()
        .to_string()
}

#[test]
fn nested_compiled_catch_prefers_innermost_handler() {
    assert_eq!(run(JitSelection::Template), run(JitSelection::InterpreterOnly));
}
