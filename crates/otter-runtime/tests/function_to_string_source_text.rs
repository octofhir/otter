//! §20.2.3.5 `Function.prototype.toString` — a user function or class
//! returns its verbatim [[SourceText]]; native, bound, and synthesized
//! callables keep the `NativeFunction` form.
//!
//! # Contents
//! - Function declarations / expressions / arrows return exact source.
//! - A class returns its whole `class … {}` definition.
//! - Concise methods, getters, and setters return their
//!   `MethodDefinition` source; a plain `key: function(){}` keeps the
//!   function-expression source.
//! - Native and bound functions render in the `NativeFunction` form.

use otter_runtime::{Runtime, SourceInput};

fn run(source: &str) -> String {
    let mut rt = Runtime::builder().build().expect("runtime");
    rt.run_script(SourceInput::from_javascript(source), "<fn-tostring>")
        .expect("script")
        .completion_string()
        .to_string()
}

#[test]
fn function_declaration_returns_source() {
    assert_eq!(
        run("function f(a, b){ return a + b; } f.toString();"),
        "function f(a, b){ return a + b; }"
    );
}

#[test]
fn function_expression_returns_source() {
    assert_eq!(
        run("var g = function (x){ return x; }; g.toString();"),
        "function (x){ return x; }"
    );
}

#[test]
fn arrow_returns_source() {
    assert_eq!(run("var a = (x) => x + 1; a.toString();"), "(x) => x + 1");
}

#[test]
fn class_returns_whole_definition() {
    assert_eq!(run("class A {}; A.toString();"), "class A {}");
}

#[test]
fn derived_class_returns_whole_definition() {
    assert_eq!(
        run("class B extends Array { m(){} }; B.toString();"),
        "class B extends Array { m(){} }"
    );
}

#[test]
fn concise_method_returns_method_definition() {
    assert_eq!(
        run("var o = { a(){ return 1; } }; o.a.toString();"),
        "a(){ return 1; }"
    );
}

#[test]
fn getter_returns_accessor_definition() {
    assert_eq!(
        run(
            "var o = { get x(){ return 2; } }; Object.getOwnPropertyDescriptor(o, 'x').get.toString();"
        ),
        "get x(){ return 2; }"
    );
}

#[test]
fn plain_property_function_keeps_function_source() {
    assert_eq!(
        run("var o = { p: function(){ return 3; } }; o.p.toString();"),
        "function(){ return 3; }"
    );
}

#[test]
fn class_instance_method_returns_method_definition() {
    assert_eq!(
        run("class C { foo(){ return 9; } }; C.prototype.foo.toString();"),
        "foo(){ return 9; }"
    );
}

#[test]
fn native_function_uses_native_form() {
    assert_eq!(
        run("Array.prototype.map.toString();"),
        "function map() { [native code] }"
    );
}

#[test]
fn bound_function_uses_native_form() {
    assert_eq!(
        run("function f(){}; f.bind(null).toString();"),
        "function () { [native code] }"
    );
}
