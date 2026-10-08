//! Offline proofs of catalog scope, finite names and current emission ownership.
//!
//! # Contents
//! - Original UTF-8 span and binding-failure observations.
//! - Typed-array reconstruction and ordered lookup recipes.
//! - Idempotent current bootstrap regeneration and exact input hashes.
//!
//! # Invariants
//! These tests parse source only. They create no runtime and execute no JS.
//! Runtime descriptor, realm and callable behavior has a separate native gate.

use std::{collections::BTreeSet, path::Path};

use oxc_ast::ast::{Argument, BindingPattern, Expression, Statement, VariableDeclarationKind};
use oxc_ast_visit::{Visit, walk};
use oxc_span::{ContentEq, GetSpan};

use super::{Coverage, Generated, collect, digest, emit, generate, parse, resolve};

const BOOTSTRAP: &str = r#"'use strict';
const constructors = { Array, ArrayBuffer, Uint8Array, Uint16Array };
const namespaces = { Math, Reflect };
const explicit = { SafeMap, ReflectApply };
const symbolWells = { SymbolIterator, SymbolToStringTag };
function __otterPrimordialBuild(primordials) {}
const primordials = { __proto__: null };
__otterPrimordialBuild(primordials);
module.exports = { primordials };
"#;

fn facts() -> resolve::Facts {
    parse(BOOTSTRAP, resolve::Facts::read).expect("current bootstrap facts")
}

#[test]
fn ambient_reads_retain_original_utf8_spans_and_default_bindings() {
    let source = "// λ🙂\nconst {ArrayIsArray: check = () => false, ['MathMax']: maximum} = primordials;\nprimordials.ArrayPrototypePush; primordials['SymbolIterator'];";
    let mut coverage = Coverage::default();
    collect::inspect("consumer.js", source, &[], &mut coverage).expect("proved ambient reads");
    assert_eq!(
        coverage
            .names
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        [
            "ArrayIsArray",
            "ArrayPrototypePush",
            "MathMax",
            "SymbolIterator",
        ]
    );
    for (name, sites) in &coverage.names {
        assert_eq!(sites.len(), 1);
        let site = &sites[0];
        let original = &source[site.start as usize..site.end as usize];
        assert!(
            original.contains(name),
            "original span for {name}: {original}"
        );
        assert_eq!(site.path, "consumer.js");
    }
    assert_eq!(coverage.names["ArrayIsArray"][0].kind, "destructure");
    assert_eq!(coverage.names["SymbolIterator"][0].kind, "literal-member");
}

#[test]
fn aliases_shadows_reassignments_rest_eval_and_dynamic_reads_fail_at_source_spans() {
    for source in [
        "const p = primordials; p.ArrayIsArray;",
        "function local(primordials) { return primordials.ArrayIsArray; }",
        "primordials = {};",
        "consume(primordials);",
        "const {...copy} = primordials;",
        "const {[name]: copy} = primordials;",
        "primordials[name];",
        "function local() { eval('primordials[name]'); }",
        "function local() { const primordials = {}; } primordials.ArrayIsArray;",
    ] {
        let error = collect::inspect("rejected.js", source, &[], &mut Coverage::default())
            .expect_err(source)
            .to_string();
        assert!(
            error.starts_with("rejected.js:"),
            "owned original source rejection: {error}"
        );
        assert!(error.contains(".."), "source byte span: {error}");
    }
}

fn dynamic_source(extra: &str, prefix: &str) -> String {
    format!(
        r#"
const {{ TypedArrayPrototypeGetSymbolToStringTag }} = primordials;
const {{ isTypedArray }} = require('internal/util/types');
{extra}
function formatRaw(ctx, value, recurseTimes, typedArray) {{
  const constructor = getConstructorName(value);
  let keys;
  if (isTypedArray(value)) {{
    {prefix}
    keys = getOwnNonIndexProperties(value, filter);
    let bound = value;
    let fallback = '';
    if (constructor === null) {{
      fallback = TypedArrayPrototypeGetSymbolToStringTag(value);
      bound = new primordials[fallback](value);
    }}
    return bound;
  }}
}}
"#
    )
}

#[test]
fn finite_reconstruction_uses_actual_const_imports_and_rejects_scope_or_mutation_drift() {
    let domain = facts().typed_arrays();
    assert_eq!(domain, ["Uint8Array", "Uint16Array"]);
    let source = dynamic_source("", "");
    let mut coverage = Coverage::default();
    collect::inspect("internal/util/inspect.js", &source, &domain, &mut coverage)
        .expect("finite reconstruction");
    assert_eq!(coverage.dynamic.len(), 1);
    let site = &coverage.dynamic[0];
    assert_eq!(
        &source[site.start as usize..site.end as usize],
        "primordials[fallback]"
    );
    for name in domain {
        assert_eq!(coverage.names[&name][0].kind, "typed-array-tag-domain");
    }
    for (extra, prefix) in [
        ("function unrelated(isTypedArray) {}", ""),
        (
            "function unrelated(TypedArrayPrototypeGetSymbolToStringTag) {}",
            "",
        ),
        ("isTypedArray = replacement;", ""),
        (
            "({TypedArrayPrototypeGetSymbolToStringTag} = replacements);",
            "",
        ),
        ("require = replacement;", ""),
        ("", "value = replacement;"),
    ] {
        collect::inspect(
            "internal/util/inspect.js",
            &dynamic_source(extra, prefix),
            &[],
            &mut Coverage::default(),
        )
        .expect_err("proof must reject actual binding or guarded-prefix drift");
    }
    collect::inspect("some/other.js", &source, &[], &mut Coverage::default())
        .expect_err("dynamic proof belongs to one existing consumer");
}

#[test]
fn phase_recipes_keep_lazy_boundaries_and_all_holder_prefix_collisions() {
    let facts = facts();
    let steps = facts.case("ArrayBufferIsViewApply").record();
    assert_eq!(
        steps["orderedSteps"],
        serde_json::json!([
            {"kind":"staticApply","authority":"constructors","base":"Array","property":"BufferIsView"},
            {"kind":"staticApply","authority":"constructors","base":"ArrayBuffer","property":"IsView"},
            {"kind":"static","authority":"constructors","base":"Array","property":"BufferIsViewApply"},
            {"kind":"static","authority":"constructors","base":"ArrayBuffer","property":"IsViewApply"},
        ])
    );
    let steps = facts.case("ArrayPrototypePushApply").record();
    assert_eq!(steps["orderedSteps"][0]["kind"], "prototypeApply");
    assert_eq!(steps["orderedSteps"][1]["kind"], "staticApply");
    assert_eq!(steps["orderedSteps"][2]["kind"], "prototypeMethod");
    assert_eq!(steps["orderedSteps"][3]["kind"], "static");
    // Each original regex phase chooses its own first complete boundary.
    let nested = facts.case("ArrayPrototypeFooPrototypeGetSize").record();
    assert_eq!(
        nested["orderedSteps"][0],
        serde_json::json!({"kind":"prototypeHalf","base":"ArrayPrototypeFoo","property":"Size","half":"get"})
    );
    assert_eq!(
        nested["orderedSteps"][1],
        serde_json::json!({"kind":"prototypeMethod","base":"Array","property":"FooPrototypeGetSize"})
    );
    assert!(facts.case("unsupported_name").steps.is_empty());
}

fn function_statements(source: &str, name: &str) -> String {
    parse(source, |program| {
        let function = program
            .body
            .iter()
            .find_map(|statement| match statement {
                Statement::FunctionDeclaration(function)
                    if function.id.as_ref().is_some_and(|id| id.name == name) =>
                {
                    Some(function)
                }
                _ => None,
            })
            .ok_or("missing helper")?;
        let body = function.body.as_ref().ok_or("missing body")?;
        let span = body.span;
        Ok(source[span.start as usize + 1..span.end as usize - 1].to_owned())
    })
    .expect("owned helper body")
}

fn assert_body(source: &str, name: &str, expected: &str) {
    let body = function_statements(source, name);
    let actual = format!("function contract() {{{body}}}");
    let expected = format!("function contract() {{{expected}}}");
    parse(&actual, |actual| {
        parse(&expected, |expected| {
            let Statement::FunctionDeclaration(actual) = &actual.body[0] else {
                panic!("actual function")
            };
            let Statement::FunctionDeclaration(expected) = &expected.body[0] else {
                panic!("expected function")
            };
            assert!(
                actual
                    .body
                    .as_ref()
                    .unwrap()
                    .statements
                    .content_eq(&expected.body.as_ref().unwrap().statements),
                "independent {name} body contract"
            );
            Ok(())
        })
    })
    .expect("helper AST contract");
}

#[test]
fn generated_helpers_preserve_symbol_fallback_and_apply_receiver_abi() {
    let cases = [
        facts().case("ArrayPrototypeGetSymbolToStringTag"),
        facts().case("ArrayPrototypePushApply"),
    ];
    let output = emit::rewrite(BOOTSTRAP, &cases).expect("static emission");
    assert_body(
        &output,
        "__otterPrimordialHalf",
        r#"
        const proto = base?.prototype;
        if (proto) {
          const fallback = symbolName && !(symbolName in symbolWells) ? symbolWells[symbolName] : key;
          const lookup = symbolName ? (symbolWells[symbolName] ?? Symbol[symbolFallback]) : key;
          const descriptor = Object.getOwnPropertyDescriptor(proto, lookup ?? fallback);
          const method = descriptor?.[half];
          if (method) return uncurryThis(method);
        }
    "#,
    );
    assert_body(
        &output,
        "__otterPrimordialApply",
        r#"
        const method = base?.prototype?.[key];
        if (typeof method === 'function') return (thisArg, args) => ReflectApply(method, thisArg, args);
    "#,
    );
    parse(&output, |program| {
        let functions = program
            .body
            .iter()
            .filter(|statement| matches!(statement, Statement::FunctionDeclaration(_)))
            .count();
        assert_eq!(
            functions, 7,
            "one build and six shared helpers, no per-case factories"
        );
        Ok(())
    })
    .expect("output parse");
}

#[derive(Default)]
struct Shape {
    guard_authorities: Vec<String>,
    static_calls: Vec<(String, String, String, String, Option<String>)>,
    missing_tokens: usize,
    bind_count: usize,
    apply_arities: BTreeSet<usize>,
    regex_count: usize,
    entries_count: usize,
}

impl<'a> Visit<'a> for Shape {
    fn visit_if_statement(&mut self, statement: &oxc_ast::ast::IfStatement<'a>) {
        if let Expression::BinaryExpression(expression) = &statement.test {
            if let Expression::Identifier(identifier) = &expression.right {
                if ["explicit", "symbolWells", "constructors", "namespaces"]
                    .contains(&identifier.name.as_str())
                {
                    self.guard_authorities.push(identifier.name.to_string());
                }
            }
        }
        walk::walk_if_statement(self, statement);
    }

    fn visit_call_expression(&mut self, call: &oxc_ast::ast::CallExpression<'a>) {
        if let Expression::Identifier(id) = &call.callee {
            if matches!(
                id.name.as_str(),
                "__otterPrimordialStatic" | "__otterPrimordialStaticApply"
            ) {
                assert_eq!(
                    call.arguments.len(),
                    3,
                    "one holder and two fixed key operands"
                );
                let Argument::ComputedMemberExpression(holder) = &call.arguments[0] else {
                    panic!("static holder is an explicit captured-table member")
                };
                let Expression::Identifier(authority) = &holder.object else {
                    panic!("captured holder authority")
                };
                let Expression::StringLiteral(base) = &holder.expression else {
                    panic!("fixed holder key")
                };
                let Argument::StringLiteral(key0) = &call.arguments[1] else {
                    panic!("fixed first member key")
                };
                let key1 = match &call.arguments[2] {
                    Argument::StringLiteral(key) => Some(key.value.to_string()),
                    Argument::NullLiteral(_) => None,
                    _ => panic!("fixed fallback key or literal null"),
                };
                self.static_calls.push((
                    id.name.to_string(),
                    authority.name.to_string(),
                    base.value.to_string(),
                    key0.value.to_string(),
                    key1,
                ));
            }
        }
        if matches!(&call.callee, Expression::Identifier(id) if id.name == "ReflectApply") {
            self.apply_arities.insert(call.arguments.len());
        }
        if let Expression::StaticMemberExpression(member) = &call.callee {
            if member.property.name == "bind" {
                self.bind_count += 1;
            }
            if member.property.name == "entries" {
                self.entries_count += 1;
            }
        }
        walk::walk_call_expression(self, call);
    }

    fn visit_variable_declaration(&mut self, declaration: &oxc_ast::ast::VariableDeclaration<'a>) {
        for variable in &declaration.declarations {
            if matches!(&variable.id, BindingPattern::BindingIdentifier(id) if id.name == "__otterPrimordialMissing")
            {
                assert_eq!(declaration.kind, VariableDeclarationKind::Const);
                assert_eq!(declaration.declarations.len(), 1);
                assert!(
                    matches!(&variable.init, Some(Expression::ObjectExpression(object)) if object.properties.is_empty())
                );
                self.missing_tokens += 1;
            }
        }
        walk::walk_variable_declaration(self, declaration);
    }

    fn visit_reg_exp_literal(&mut self, _: &oxc_ast::ast::RegExpLiteral<'a>) {
        self.regex_count += 1;
    }
}

#[test]
fn emitted_static_lookup_stops_on_undefined_and_has_no_generic_decoder() {
    let output = emit::rewrite(
        BOOTSTRAP,
        &[
            facts().case("ArrayBufferIsView"),
            facts().case("MathMaxApply"),
        ],
    )
    .expect("emission");
    let mut shape = Shape::default();
    parse(&output, |program| {
        shape.visit_program(program);
        Ok(())
    })
    .expect("output AST");
    assert!(
        shape.guard_authorities.is_empty(),
        "holder priority is chosen by the producer, not guarded at run time"
    );
    assert_eq!(
        shape.static_calls,
        [
            (
                "__otterPrimordialStatic",
                "constructors",
                "Array",
                "bufferIsView",
                Some("BufferIsView")
            ),
            (
                "__otterPrimordialStatic",
                "constructors",
                "ArrayBuffer",
                "isView",
                Some("IsView")
            ),
            (
                "__otterPrimordialStaticApply",
                "namespaces",
                "Math",
                "max",
                Some("Max")
            ),
            (
                "__otterPrimordialStatic",
                "namespaces",
                "Math",
                "maxApply",
                Some("MaxApply")
            ),
        ]
        .map(|(helper, authority, base, key0, key1)| (
            helper.to_owned(),
            authority.to_owned(),
            base.to_owned(),
            key0.to_owned(),
            key1.map(str::to_owned)
        ))
    );
    assert_eq!(shape.bind_count, 1, "one shared binding owner");
    assert_eq!(
        shape.missing_tokens, 1,
        "one private literal token per module"
    );
    assert_eq!(shape.apply_arities, BTreeSet::from([3]));
    assert_eq!(shape.regex_count, 0);
    assert_eq!(shape.entries_count, 0);
}

#[test]
fn holder_table_names_read_the_first_holding_table() {
    let facts = facts();
    assert_eq!(facts.holder_of("ReflectApply"), Some("explicit"));
    assert_eq!(facts.holder_of("SymbolIterator"), Some("symbolWells"));
    assert_eq!(facts.holder_of("Uint8Array"), Some("constructors"));
    assert_eq!(facts.holder_of("Math"), Some("namespaces"));
    assert_eq!(facts.holder_of("ArrayIsArray"), None);
    let output = emit::rewrite(BOOTSTRAP, &[facts.case("ReflectApply")]).expect("emission");
    assert!(output.contains(
        r#"value=explicit["ReflectApply"]; if(value!==undefined)primordials["ReflectApply"]=value;"#
    ));
    assert!(!output.contains("namespaces[\"Reflect\"]"));
}

#[test]
fn compact_static_helpers_preserve_single_reads_and_present_undefined() {
    let cases = [facts().case("MathMAX"), facts().case("MathMAXApply")];
    let output = emit::rewrite(BOOTSTRAP, &cases).expect("static emission");
    assert_body(
        &output,
        "__otterPrimordialStatic",
        r#"
        if (!holder) return __otterPrimordialMissing;
        let key;
        if (key0 in holder) key = key0;
        else if (key1 !== null && key1 in holder) key = key1;
        else return __otterPrimordialMissing;
        const member = holder[key];
        return typeof member === 'function' ? member.bind(holder) : member;
    "#,
    );
    assert_body(
        &output,
        "__otterPrimordialStaticApply",
        r#"
        if (holder) {
          const member = key1 === null ? holder[key0] : (holder[key0] ?? holder[key1]);
          if (typeof member === 'function') return (args) => ReflectApply(member, holder, args);
        }
    "#,
    );
    let mut shape = Shape::default();
    parse(&output, |program| {
        shape.visit_program(program);
        Ok(())
    })
    .expect("compact all-caps calls");
    assert_eq!(shape.static_calls.len(), 3);
    assert_eq!(shape.static_calls[0].3, "MAX");
    assert_eq!(shape.static_calls[0].4, None, "single static getter read");
    assert_eq!(shape.static_calls[1].3, "MAX");
    assert_eq!(shape.static_calls[1].4, None, "single Apply getter read");
    assert_eq!(shape.static_calls[2].3, "mAXApply");
    assert_eq!(shape.static_calls[2].4.as_deref(), Some("MAXApply"));
}

#[test]
fn current_dispatch_regenerates_and_rejects_private_token_or_helper_escape() {
    let cases = [facts().case("ArrayIsArray"), facts().case("MathMax")];
    let output = emit::rewrite(BOOTSTRAP, &cases).expect("first emission");
    assert_eq!(
        emit::rewrite(&output, &cases).expect("sole current emission"),
        output
    );
    assert_eq!(
        output
            .split("const __otterPrimordialMissing")
            .next()
            .unwrap(),
        BOOTSTRAP
            .split("function __otterPrimordialBuild")
            .next()
            .unwrap(),
        "every byte before the AST-owned group is preserved"
    );
    assert!(output.ends_with(BOOTSTRAP.split("const primordials").last().unwrap()));
    for escaped in [
        "consume(__otterPrimordialMissing);",
        "module.exports.missing = __otterPrimordialMissing;",
        "consume(__otterPrimordialStatic);",
        "const __otterPrimordialObject = replacement;",
        "const __otterPrimordialMissing = {};",
    ] {
        emit::rewrite(&(output.clone() + escaped), &cases)
            .expect_err("private owner must not escape or collide");
    }
    for malformed in [
        "const __otterPrimordialMissing = Symbol();",
        "let __otterPrimordialMissing = {};",
        "const __otterPrimordialMissing = { exposed: true };",
    ] {
        let malformed = output.replacen("const __otterPrimordialMissing = {};", malformed, 1);
        emit::rewrite(&malformed, &cases).expect_err("one const literal token only");
    }
}

#[test]
fn producer_manifest_has_exact_macro_sources_and_deterministic_owned_catalog() {
    let temp = tempfile::tempdir().expect("fixture directory");
    let node = temp.path().join("crates/otter-node/src");
    std::fs::create_dir_all(node.join("nodelib/compat")).expect("fixture source tree");
    let declarations = "builtin_table! { installer bootstrap \"realm\" plain \"nodelib/compat/bootstrap_realm.js\"; installer consumer \"actual\" vendored \"nodelib/consumer.js\"; installer own \"own\" plain \"own.js\"; }";
    let consumer = "const {ArrayIsArray} = primordials; return primordials.MathMax;";
    std::fs::write(node.join("builtin_table.rs"), declarations).expect("declarations");
    let bootstrap = node.join("nodelib/compat/bootstrap_realm.js");
    std::fs::write(&bootstrap, BOOTSTRAP).expect("bootstrap");
    std::fs::write(node.join("nodelib/consumer.js"), consumer).expect("consumer");
    // An undeclared file cannot become an implicit runtime or tool producer.
    std::fs::write(node.join("nodelib/inactive.js"), "primordials[unknown]")
        .expect("inactive fixture");
    let generated = generate(temp.path(), &bootstrap).expect("current registered sources");
    let repeat = generate(temp.path(), &bootstrap).expect("deterministic repeat");
    assert_eq!(generated.source, repeat.source);
    assert_eq!(
        generated.manifest_text().unwrap(),
        repeat.manifest_text().unwrap()
    );
    assert_eq!(
        generated.manifest["sourceSha256"]["../builtin_table.rs"],
        digest(declarations)
    );
    assert_eq!(
        generated.manifest["sourceSha256"]["consumer.js"],
        digest(consumer)
    );
    assert!(
        generated.manifest["sourceSha256"]
            .get("inactive.js")
            .is_none()
    );
    assert_eq!(
        generated.manifest["bootstrapOutputSha256"],
        digest(&generated.source)
    );
    std::fs::write(&bootstrap, &generated.source).expect("current generated input");
    let current = generate(temp.path(), &bootstrap).expect("regenerate sole current owner");
    assert_eq!(current.source, generated.source);
    assert_eq!(current.manifest["catalog"], generated.manifest["catalog"]);
    assert_eq!(current.manifest["recipes"], generated.manifest["recipes"]);
    let (owner, paths) = collect::source_paths(temp.path()).expect("AST producer census");
    assert_eq!(owner, declarations);
    assert_eq!(paths.len(), 2);
    std::fs::write(
        node.join("builtin_table.rs"),
        "builtin_table! { installer bad \"bad\" vendored \"../outside.js\"; }",
    )
    .expect("escaping declaration");
    collect::source_paths(temp.path()).expect_err("source owner path escape");
}

#[test]
fn actual_registered_sources_are_a_closed_reproducible_ast_catalog() {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root");
    let bootstrap = repo.join("crates/otter-node/src/nodelib/compat/bootstrap_realm.js");
    let generated =
        generate(repo, &bootstrap).expect("all current registered source bindings and spans");
    assert_eq!(generated.manifest["catalog"].as_array().unwrap().len(), 277);
    assert_eq!(
        generated.manifest["sourceSha256"]
            .as_object()
            .unwrap()
            .len(),
        192
    );
    let current = std::fs::read_to_string(&bootstrap).expect("actual bootstrap");
    let (before_start, before_end) = dispatch_span(&current);
    let (after_start, after_end) = dispatch_span(&generated.source);
    assert_eq!(
        &current[..before_start],
        &generated.source[..after_start],
        "captured intrinsics/Safe tables prefix unchanged"
    );
    assert_eq!(
        &current[before_end..],
        &generated.source[after_end..],
        "primordials/bindings/export suffix unchanged"
    );
    assert_eq!(
        generated.manifest["dynamicReads"].as_array().unwrap().len(),
        1
    );
    let facts = facts_from(&generated.source);
    let cases = generated.manifest["catalog"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| facts.case(row["name"].as_str().unwrap()))
        .collect::<Vec<_>>();
    assert_eq!(
        emit::rewrite(&generated.source, &cases).unwrap(),
        generated.source
    );
}

fn dispatch_span(source: &str) -> (usize, usize) {
    parse(source, |program| {
        let mut spans = Vec::new();
        for statement in &program.body {
            match statement {
                Statement::FunctionDeclaration(function)
                    if function.id.as_ref().is_some_and(|id| id.name.starts_with("__otterPrimordial")) => spans.push(function.span),
                Statement::VariableDeclaration(declaration)
                    if declaration.declarations.iter().any(|binding| matches!(&binding.id, BindingPattern::BindingIdentifier(id) if id.name == "__otterPrimordialMissing")) => spans.push(statement.span()),
                _ => {}
            }
        }
        spans.sort_by_key(|span| span.start);
        Ok((spans.first().ok_or("dispatch start")?.start as usize, spans.last().ok_or("dispatch end")?.end as usize))
    }).expect("actual AST-owned dispatch span")
}

fn facts_from(source: &str) -> resolve::Facts {
    parse(source, resolve::Facts::read).expect("current emitted facts")
}

#[test]
fn owned_records_do_not_retain_ast_arenas() {
    fn send_sync<T: Send + Sync>() {}
    send_sync::<Coverage>();
    send_sync::<Generated>();
    send_sync::<resolve::Case>();
}
