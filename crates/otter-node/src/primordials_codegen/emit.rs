//! Idempotent AST-span emission of the sole static primordial dispatcher.
//!
//! # Contents
//! - Compact cases and six shared lookup helpers.
//! - One private missing token and current generated-span ownership.
//! - Exact UTF-8 byte-span replacement without regex source parsing.
//!
//! # Invariants
//! Static members are read live. A present undefined static property stops
//! lookup. Helpers create no per-name values until the existing lazy cache
//! requests them; their bound/apply callable ABI is the original one. One
//! module-private literal object represents absence without confusing a
//! present undefined property with a missing member. It cannot escape the
//! generated span. All-caps static members have exactly one getter read.
//!
//! # See also
//! - `resolve` selects all original stage and prefix-order candidates.

use oxc_ast::ast::{BindingPattern, Expression, Program, Statement, VariableDeclarationKind};
use oxc_ast_visit::{Visit, walk};
use oxc_span::{GetSpan, Span};

use super::{
    Result, parse, reject,
    resolve::{Case, Step},
};

const MISSING: &str = "__otterPrimordialMissing";

const HELPERS: [&str; 6] = [
    "__otterPrimordialObject",
    "__otterPrimordialHalf",
    "__otterPrimordialApply",
    "__otterPrimordialMethod",
    "__otterPrimordialStatic",
    "__otterPrimordialStaticApply",
];

fn quote(value: &str) -> String {
    // A JSON string literal is also an exact JavaScript string literal.
    serde_json::Value::String(value.to_owned()).to_string()
}

fn lower(value: &str) -> String {
    let mut chars = value.chars();
    chars
        .next()
        .map(|first| first.to_ascii_lowercase().to_string() + chars.as_str())
        .unwrap_or_default()
}

fn upper(value: &str) -> bool {
    value.to_ascii_uppercase() == value
}

fn base(name: &str, object: bool) -> String {
    let base = format!("constructors[{}]", quote(name));
    match name {
        "TypedArray" if object => format!("({base} ?? TypedArray)"),
        "TypedArray" => "TypedArray".to_owned(),
        "AsyncGenerator" if object => {
            format!("({base} ?? AsyncGeneratorFunction.prototype.prototype)")
        }
        _ => base,
    }
}

fn symbol_args(property: &str) -> String {
    if let Some(rest) = property.strip_prefix("Symbol") {
        format!(
            "{},{},{}",
            quote(&lower(property)),
            quote(property),
            quote(&lower(rest))
        )
    } else {
        format!("{},null,null", quote(&lower(property)))
    }
}

fn static_keys(property: &str) -> String {
    if upper(property) {
        format!("{},null", quote(property))
    } else {
        format!("{},{}", quote(&lower(property)), quote(property))
    }
}

fn dispatch(cases: &[Case]) -> String {
    let mut out = String::from(
        "const __otterPrimordialMissing = {};\n\nfunction derive(name) {\n  // Generated from the current in-repo AST catalog; members remain lazy/live.\n  if (name in explicit) return explicit[name];\n  if (name in symbolWells) return symbolWells[name];\n  if (name in constructors) return constructors[name];\n  if (name in namespaces) return namespaces[name];\n  let value;\n  switch (name) {\n",
    );
    for case in cases {
        if case.steps.is_empty() {
            continue;
        }
        out.push_str(&format!("    case {}:\n", quote(&case.name)));
        for step in &case.steps {
            let text = match step {
                Step::PrototypeObject { base: name } => format!(
                    "value=__otterPrimordialObject({}); if(value!==undefined)return value;",
                    base(name, true)
                ),
                Step::PrototypeHalf {
                    base: name,
                    property,
                    half,
                } => format!(
                    "value=__otterPrimordialHalf({},{},{}); if(value!==undefined)return value;",
                    base(name, false),
                    symbol_args(property),
                    quote(half)
                ),
                Step::PrototypeApply {
                    base: name,
                    property,
                } => format!(
                    "value=__otterPrimordialApply({},{}); if(value!==undefined)return value;",
                    base(name, false),
                    quote(&lower(property))
                ),
                Step::PrototypeMethod {
                    base: name,
                    property,
                } => format!(
                    "value=__otterPrimordialMethod({},{}); if(value!==undefined)return value;",
                    base(name, false),
                    symbol_args(property)
                ),
                Step::StaticApply {
                    authority,
                    base,
                    property,
                } => format!(
                    "value=__otterPrimordialStaticApply({authority}[{}],{}); if(value!==undefined)return value;",
                    quote(base),
                    static_keys(property)
                ),
                Step::Static {
                    authority,
                    base,
                    property,
                } => format!(
                    "value=__otterPrimordialStatic({authority}[{}],{}); if(value!==__otterPrimordialMissing)return value;",
                    quote(base),
                    static_keys(property)
                ),
            };
            out.push_str("      ");
            out.push_str(&text);
            out.push('\n');
        }
        out.push_str("      return undefined;\n");
    }
    out.push_str("  }\n  return undefined;\n}\n\n");
    out.push_str(
        "function __otterPrimordialObject(base) {\n  if(base)return base.prototype??base;\n}\n\n",
    );
    out.push_str("function __otterPrimordialHalf(base,key,symbolName,symbolFallback,half) {\n  const proto=base?.prototype;\n  if(proto){\n    const fallback=symbolName && !(symbolName in symbolWells)?symbolWells[symbolName]:key;\n    const lookup=symbolName?(symbolWells[symbolName]??Symbol[symbolFallback]):key;\n    const descriptor=Object.getOwnPropertyDescriptor(proto,lookup??fallback);\n    const method=descriptor?.[half];\n    if(method)return uncurryThis(method);\n  }\n}\n\n");
    out.push_str("function __otterPrimordialApply(base,key) {\n  const method=base?.prototype?.[key];\n  if(typeof method==='function')return (thisArg,args)=>ReflectApply(method,thisArg,args);\n}\n\n");
    out.push_str("function __otterPrimordialMethod(base,key,symbolName,symbolFallback) {\n  const proto=base?.prototype;\n  if(proto){\n    const lookup=symbolName?(symbolWells[symbolName]??Symbol[symbolFallback]):key;\n    const method=proto[lookup];\n    if(typeof method==='function')return uncurryThis(method);\n    const descriptor=Object.getOwnPropertyDescriptor(proto,lookup);\n    if(descriptor?.get)return uncurryThis(descriptor.get);\n  }\n}");
    out.push_str("\n\nfunction __otterPrimordialStatic(holder,key0,key1) {\n  if(!holder)return __otterPrimordialMissing;\n  let key;\n  if(key0 in holder)key=key0;\n  else if(key1!==null && key1 in holder)key=key1;\n  else return __otterPrimordialMissing;\n  const member=holder[key];\n  return typeof member==='function'?member.bind(holder):member;\n}\n\n");
    out.push_str("function __otterPrimordialStaticApply(holder,key0,key1) {\n  if(holder){\n    const member=key1===null?holder[key0]:(holder[key0]??holder[key1]);\n    if(typeof member==='function')return (args)=>ReflectApply(member,holder,args);\n  }\n}");
    out
}

struct Refs {
    owned: Span,
    errors: Vec<Span>,
}

impl<'a> Visit<'a> for Refs {
    fn visit_identifier_reference(&mut self, identifier: &oxc_ast::ast::IdentifierReference<'a>) {
        if (identifier.name == MISSING || HELPERS.contains(&identifier.name.as_str()))
            && (identifier.span.start < self.owned.start || identifier.span.end > self.owned.end)
        {
            self.errors.push(identifier.span);
        }
    }

    fn visit_binding_identifier(&mut self, identifier: &oxc_ast::ast::BindingIdentifier<'a>) {
        if (identifier.name == MISSING || HELPERS.contains(&identifier.name.as_str()))
            && (identifier.span.start < self.owned.start || identifier.span.end > self.owned.end)
        {
            self.errors.push(identifier.span);
        }
        walk::walk_binding_identifier(self, identifier);
    }
}

fn edits(program: &Program<'_>) -> Result<Span> {
    let mut spans = Vec::new();
    let mut derive_count = 0;
    let mut helpers = std::collections::BTreeSet::new();
    let mut missing = false;
    for statement in &program.body {
        if let Statement::FunctionDeclaration(function) = statement {
            if let Some(identifier) = &function.id {
                if identifier.name == "derive" {
                    derive_count += 1;
                    spans.push(function.span);
                } else if HELPERS.contains(&identifier.name.as_str()) {
                    if !helpers.insert(identifier.name.as_str()) {
                        return Err(reject(
                            "bootstrap",
                            function.span,
                            "duplicate current dispatch helper",
                        ));
                    }
                    spans.push(function.span);
                }
            }
        }
        if let Statement::VariableDeclaration(declaration) = statement {
            for variable in &declaration.declarations {
                if let BindingPattern::BindingIdentifier(identifier) = &variable.id {
                    if identifier.name == MISSING {
                        if missing
                            || declaration.kind != VariableDeclarationKind::Const
                            || declaration.declarations.len() != 1
                            || !matches!(&variable.init, Some(Expression::ObjectExpression(object)) if object.properties.is_empty())
                        {
                            return Err(reject(
                                "bootstrap",
                                declaration.span,
                                "private missing token must be one const empty object literal",
                            ));
                        }
                        missing = true;
                        spans.push(declaration.span);
                    }
                }
            }
        }
    }
    if derive_count != 1 {
        return Err("expected one current derive authority".into());
    }
    spans.sort_by_key(|span| span.start);
    let owned = Span::new(
        spans[0].start,
        spans.last().ok_or("missing dispatch span")?.end,
    );
    for statement in &program.body {
        let span = statement.span();
        if span.start >= owned.start && span.end <= owned.end && !spans.contains(&span) {
            return Err(reject(
                "bootstrap",
                span,
                "dispatch group contains another current source owner",
            ));
        }
    }
    let mut references = Refs {
        owned,
        errors: Vec::new(),
    };
    references.visit_program(program);
    if let Some(span) = references.errors.first() {
        return Err(reject(
            "bootstrap",
            *span,
            "private dispatch helper or missing token is used outside its owner",
        ));
    }
    Ok(owned)
}

pub(super) fn rewrite(source: &str, cases: &[Case]) -> Result<String> {
    let owned = parse(source, edits)?;
    let mut output = String::new();
    output.push_str(
        source
            .get(..owned.start as usize)
            .ok_or("invalid source prefix")?,
    );
    output.push_str(&dispatch(cases));
    output.push_str(
        source
            .get(owned.end as usize..)
            .ok_or("invalid source suffix")?,
    );
    parse(&output, |_| Ok(()))?;
    Ok(output)
}
