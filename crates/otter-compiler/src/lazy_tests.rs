//! A lazily compiled body lowers to the same bytecode its eager compile
//! produces: same instructions, scopes, handlers and source spans, with
//! constants compared by value and function references by source span.

use super::*;
use otter_bytecode::{EvalCallerChain, EvalCallerScope};

const LAZY: CompileOptions = CompileOptions {
    lazy_functions: true,
};

/// One function's code in a form independent of pool and table layout.
fn render(module: &BytecodeModule, function: &Function) -> String {
    let mut out = format!(
        "{} span={:?} regs={}+{} params={} len={} strict={} arrow={} method={} args={} eval={}\n",
        function.name,
        function.span,
        function.locals,
        function.scratch,
        function.param_count,
        function.length,
        function.is_strict,
        function.is_arrow,
        function.is_method,
        function.needs_arguments,
        function.contains_direct_eval,
    );
    for scope in &function.scopes {
        out.push_str(&format!("  scope {:?}\n", scope));
    }
    for instr in function.code.iter() {
        out.push_str(&format!("  {}", instr.op.mnemonic()));
        for (index, operand) in function.code.operands(instr).iter().enumerate() {
            match operand {
                Operand::ConstIndex(k) if instr.op.is_const_pool_operand(index) => {
                    match module.constants.get(k as usize) {
                        Some(Constant::FunctionId { index }) => {
                            let target = &module.functions[*index as usize];
                            out.push_str(&format!(" fn@{:?}", target.span));
                        }
                        Some(other) => out.push_str(&format!(" {other:?}")),
                        None => out.push_str(" k?"),
                    }
                }
                other => out.push_str(&format!(" {other:?}")),
            }
        }
        out.push('\n');
    }
    for handler in &function.handlers {
        out.push_str(&format!("  handler {handler:?}\n"));
    }
    for entry in function.spans.iter() {
        out.push_str(&format!("  span {} {:?}\n", entry.pc, entry.span));
    }
    out
}

/// Compile `source` eagerly and lazily, compile every top-level lazy
/// record's body, and require each to match its eager compile.
fn assert_lazy_matches_eager(source: &str) -> usize {
    // The eager reference compiles the top-level functions now and
    // leaves their nested functions lazy, as a body compile does.
    let eager = otter_syntax::with_program_goal(
        source,
        SyntaxSourceKind::JavaScript,
        otter_syntax::SourceGoal::Script,
        |program| {
            crate::entry::compile_program(
                program,
                SyntaxSourceKind::JavaScript,
                "test.js",
                false,
                Some(crate::compiler::LazyUnit {
                    module_goal: false,
                    typescript: false,
                    jsx: false,
                    top_level_eager: true,
                }),
            )
        },
    )
    .unwrap()
    .unwrap();
    let lazy = compile_script_source_to_module_with_options(
        source,
        SyntaxSourceKind::JavaScript,
        "test.js",
        LAZY,
    )
    .unwrap()
    .bytecode;
    // A top-level function closes over `<main>`'s first context, if any.
    let chain = EvalCallerChain {
        scopes: lazy.functions[0]
            .scopes
            .first()
            .map(|descriptor| EvalCallerScope {
                descriptor: descriptor.clone().into(),
                extension_names: Vec::new(),
            })
            .into_iter()
            .collect(),
        var_depth: None,
    };
    let mut compared = 0;
    for record in lazy
        .functions
        .iter()
        .filter(|function| function.lazy.is_some())
    {
        let body = compile_lazy_function(LazyCompileRequest {
            source,
            module_url: "test.js",
            function: record,
            chain: &chain,
            environment: lazy.lazy_environments.first(),
            resolutions: &lazy.module_resolutions,
        })
        .unwrap_or_else(|error| panic!("lazy compile of {}: {error:?}", record.name));
        let eager_function = eager
            .functions
            .iter()
            .find(|function| function.span == record.span)
            .unwrap_or_else(|| panic!("no eager compile of {}", record.name));
        let lazily = render(&body, &body.functions[0]);
        let eagerly = render(&eager, eager_function);
        assert_eq!(
            lazily, eagerly,
            "lazy compile of `{}` diverges",
            record.name
        );
        compared += 1;
    }
    compared
}

#[test]
fn top_level_functions_compile_lazily_to_their_eager_code() {
    let compared = assert_lazy_matches_eager(
        r#"
function add(a, b) { return a + b; }
function rec(n, acc) { if (n <= 0) return acc; return rec(n - 1, acc + n); }
function args() { return arguments.length + arguments[1]; }
function sloppyThis() { return typeof this; }
function closures(x) {
  let y = x * 2;
  const inner = (z) => y + z;
  function deep(k) { return inner(k) * 2; }
  return deep(1);
}
function tryCatch(f) { try { return f(); } catch (e) { return e; } finally { add(1, 2); } }
function* gen(a) { yield a; yield* [1, 2]; }
async function asy(p) { return await p; }
function withDefault(a, b = a + 1, ...rest) { return [a, b, rest]; }
function destructure({ a, b: [c] }, [d = 4]) { return a + c + d; }
function classes() { class C { #x = 1; m() { return this.#x; } static s() {} } return new C(); }
function objects(k) { return { k, [k]: 1, m() { return super.toString; }, get g() { return 1; } }; }
var named = function self(n) { return n ? self(n - 1) : 0; };
"#,
    );
    assert_eq!(compared, 13);
}

#[test]
fn a_function_closing_over_top_level_lexicals_compiles_lazily_to_its_eager_code() {
    let compared = assert_lazy_matches_eager(
        r#"
let counter = 0;
const step = 2;
function bump() { counter += step; return counter; }
function read() { return counter; }
"#,
    );
    assert_eq!(compared, 2);
}

#[test]
fn immediately_called_and_parenthesized_functions_compile_eagerly() {
    let lazy = compile_script_source_to_module_with_options(
        "(function () { return 1; })(); !function () {}(); (() => 2)(); function g() {}",
        SyntaxSourceKind::JavaScript,
        "test.js",
        LAZY,
    )
    .unwrap()
    .bytecode;
    let lazy_names: Vec<&str> = lazy
        .functions
        .iter()
        .filter(|function| function.lazy.is_some())
        .map(|function| function.name.as_str())
        .collect();
    assert_eq!(lazy_names, ["g"]);
}

#[test]
fn globals_reaching_functions_compile_lazily_to_their_eager_code() {
    let compared = assert_lazy_matches_eager(
        r#"
globalThis.__next = 1;
function recursiveA(n, retained) {
  if (n <= 0) return retained;
  const next = n === 2 ? globalThis.__next : n - 1;
  return recursiveB(next, retained);
}
function recursiveB(n, retained) {
  if (n <= 0) return retained;
  return recursiveA(n - 1, retained);
}
globalThis.__coercible = { valueOf() { return 0; } };
"#,
    );
    assert_eq!(compared, 3);
}

#[test]
fn a_body_compile_names_its_functions_after_the_module_the_vm_found() {
    // A script compiled under a non-file specifier leaves its functions'
    // URL empty until it links; a body compiled from such a record must
    // still name its nested records after the module, or their own body
    // compiles cannot find the source.
    let source = "function outer() { return function inner() { return 1; }; }";
    let lazy = compile_script_source_to_module_with_options(
        source,
        SyntaxSourceKind::JavaScript,
        "https://example.test/page.js",
        LAZY,
    )
    .unwrap()
    .bytecode;
    let record = lazy
        .functions
        .iter()
        .find(|function| function.name == "outer")
        .unwrap();
    assert_eq!(record.module_url, "");
    let body = compile_lazy_function(LazyCompileRequest {
        source,
        module_url: "https://example.test/page.js",
        function: record,
        chain: &EvalCallerChain::default(),
        environment: None,
        resolutions: &[],
    })
    .unwrap();
    assert_eq!(body.module, "https://example.test/page.js");
    let inner = body
        .functions
        .iter()
        .find(|function| function.name == "inner")
        .unwrap();
    assert!(inner.lazy.is_some());
    assert_eq!(inner.module_url, "https://example.test/page.js");
}

/// A preparse tree with its names spelled and its lists sorted, so trees
/// built by different routes compare by content.
fn spelled(preparse: Option<&otter_bytecode::Preparse>) -> String {
    fn function(names: &[String], f: &otter_bytecode::SkippedFunction, out: &mut String) {
        let spell = |ids: &[u32]| {
            let mut v: Vec<&str> = ids.iter().map(|&id| names[id as usize].as_str()).collect();
            v.sort_unstable();
            v.join(",")
        };
        out.push_str(&format!(
            "[{:?} {:?} esc={} wr={} ee={} ce={} ",
            f.span,
            f.blank,
            spell(&f.escape),
            spell(&f.escape_writes),
            f.escape_eval,
            f.contains_eval
        ));
        for child in &f.children {
            function(names, child, out);
        }
        out.push(']');
    }
    let mut out = String::new();
    if let Some(preparse) = preparse {
        for f in &preparse.skipped {
            function(&preparse.names, f, &mut out);
        }
    }
    out
}

/// Compile every lazy record of `source`, at every depth, with its preparse
/// facts and without them: the bodies must lower identically and hand
/// their own lazy records identical facts. Returns how many bodies left
/// functions unparsed.
fn assert_skipping_changes_nothing(source: &str) -> usize {
    let unit = compile_script_source_to_module_with_options(
        source,
        SyntaxSourceKind::JavaScript,
        "test.js",
        LAZY,
    )
    .unwrap()
    .bytecode;
    let top_chain = EvalCallerChain {
        scopes: unit.functions[0]
            .scopes
            .first()
            .map(|descriptor| EvalCallerScope {
                descriptor: descriptor.clone().into(),
                extension_names: Vec::new(),
            })
            .into_iter()
            .collect(),
        var_depth: None,
    };
    let mut work: Vec<(Function, EvalCallerChain)> = unit
        .functions
        .iter()
        .filter(|function| function.lazy.is_some())
        .map(|function| (function.clone(), top_chain.clone()))
        .collect();
    let mut skipping = 0;
    while let Some((record, chain)) = work.pop() {
        let compile = |record: &Function| {
            compile_lazy_function(LazyCompileRequest {
                source,
                module_url: "test.js",
                function: record,
                chain: &chain,
                environment: unit.lazy_environments.first(),
                resolutions: &unit.module_resolutions,
            })
            .unwrap_or_else(|error| panic!("body compile of {}: {error:?}", record.name))
        };
        let mut bare = record.clone();
        bare.preparse = None;
        skipping += usize::from(record.preparse.is_some());
        let with = compile(&record);
        let without = compile(&bare);
        assert_eq!(
            render(&with, &with.functions[0]),
            render(&without, &without.functions[0]),
            "skipping changes the body of `{}`",
            record.name
        );
        let nested = |module: &BytecodeModule| -> Vec<(String, String)> {
            module
                .functions
                .iter()
                .filter(|function| function.lazy.is_some())
                .map(|function| (function.name.clone(), spelled(function.preparse.as_deref())))
                .collect()
        };
        assert_eq!(
            nested(&with),
            nested(&without),
            "skipping changes the facts `{}` hands its functions",
            record.name
        );
        // The body's own scopes stand in for the context its functions close
        // over; both compiles of each see the same chain.
        let inner = EvalCallerChain {
            scopes: with.functions[0]
                .scopes
                .iter()
                .rev()
                .map(|descriptor| EvalCallerScope {
                    descriptor: descriptor.clone().into(),
                    extension_names: Vec::new(),
                })
                .chain(chain.scopes.iter().cloned())
                .collect(),
            var_depth: None,
        };
        for function in with
            .functions
            .iter()
            .filter(|function| function.lazy.is_some())
        {
            work.push((function.clone(), inner.clone()));
        }
    }
    skipping
}

#[test]
fn a_body_compile_that_skips_inner_functions_lowers_as_if_it_parsed_them() {
    // Bodies long enough to skip, nested three deep, reaching outward by
    // read, write, self name, eval, switch and through arrows.
    let filler = (0..12)
        .map(|i| format!("  local{i} = local{i} + {i} * shared;\n"))
        .collect::<String>();
    let source = format!(
        r#"
var shared = 1;
function outer(a, b) {{
  let counter = 0, local0 = 0, local1 = 0, local2 = 0, local3 = 0, local4 = 0, local5 = 0;
  let local6 = 0, local7 = 0, local8 = 0, local9 = 0, local10 = 0, local11 = 0;
  const named = function self(n) {{
{filler}    function deeper(k) {{
{filler}      counter += k;
      return n ? self(n - 1) : a + k;
    }}
    return deeper(n);
  }};
  function writer() {{
{filler}    b = counter;
    switch (counter) {{ case 1: return local1; default: return shared; }}
  }}
  function evaluator(src) {{
{filler}    return eval(src);
  }}
  const arrow = (x) => {{
    function insideArrow() {{
{filler}      return x + arguments.length + counter;
    }}
    return insideArrow;
  }};
  var iife = (function () {{
{filler}    return counter;
  }})();
  return [named, writer, evaluator, arrow, iife];
}}
"#
    );
    let skipping = assert_skipping_changes_nothing(&source);
    assert!(skipping >= 2, "bodies that skip: {skipping}");
}
