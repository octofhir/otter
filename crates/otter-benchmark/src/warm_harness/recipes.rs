//! Complete immutable workload recipes and deterministic persistent resets.
//!
//! # Contents
//! - The seven original full driver AST contracts.
//! - Crypto pool replay, TypeScript input restoration and zlib lifetime/reset.
//! - Decoded zlib integrity-writer proof and original useful-check spans.
//!
//! # Invariants
//! - Source hashes and structural AST comparison both precede emission.
//! - Original algorithm/data bytes are copied unchanged at Script scope.
//! - Useful inner checks remain part of the original work.
//!
//! # See also
//! - `super::emit` owns the common timing/output window.

use super::{
    WarmAnchor, WarmCheckValue, WarmEmbeddedScript, WarmHarnessError, WarmPieceRole,
    WarmSemanticResult, WarmSource, WarmSourcePiece, ast, reject, source_sha256,
};
use oxc_ast::ast::{Argument, Expression, ObjectProperty, Program, Statement};
use oxc_ast_visit::{Visit, walk};
use oxc_span::{ContentEq, GetSpan, Span};
use std::collections::BTreeMap;

pub(super) struct Plan {
    pub setup: String,
    pub saved: String,
    pub reset: String,
    pub precheck: String,
    pub work: String,
    pub check: String,
    pub cleanup: String,
    pub final_cleanup: String,
    pub pieces: Vec<WarmSourcePiece>,
    pub embedded: Vec<WarmEmbeddedScript>,
    pub data: BTreeMap<String, String>,
    pub contract: BTreeMap<String, String>,
    pub expected: Option<WarmSemanticResult>,
    pub required: BTreeMap<String, WarmCheckValue>,
}

pub(super) fn prepare(source: &WarmSource) -> Result<Plan, WarmHarnessError> {
    ast::parse(&source.original, |program| prepare_program(source, program))
}

fn prepare_program(source: &WarmSource, program: &Program<'_>) -> Result<Plan, WarmHarnessError> {
    ast::check_original(program)?;
    let template = match source.anchor {
        WarmAnchor::Fib => "for (let i=0;i<5;i++) s+=fib(30); console.log(s);",
        WarmAnchor::MegaMethod => {
            "for(let r=0;r<20;r++) total=(total+run(1000000))|0; console.log(total);"
        }
        WarmAnchor::AstCtor => "for(var r=0;r<20;r++) total+=run(100000); console.log(total);",
        WarmAnchor::Typescript => {
            "setupTypescript(); for(let i=0;i<8;i++)runTypescript(); tearDownTypescript();"
        }
        WarmAnchor::Crypto => "for(let i=0;i<60;i++){encrypt();decrypt();} console.log(encrypted);",
        WarmAnchor::Zlib => {
            "for(let i=0;i<12;i++)runZlib(); if(Module.zlibIntegrityComparisons!==720 || Module.zlibIntegrityBytes!==72000000 || Module.zlibIntegrityInputByteSum!==2773014){throw new Error(\"Zlib validation work count or checksum mismatch\");} console.log(\"zlib-ok:\"+Module.zlibIntegrityComparisons+\":\"+Module.zlibIntegrityBytes+\":\"+Module.zlibIntegrityInputByteSum);tearDownZlib();"
        }
        WarmAnchor::EarleyBoyer => {
            "for(const suite of BenchmarkSuite.suites){for(const benchmark of suite.benchmarks){benchmark.Setup();const iterations=benchmark.deterministicIterations*1;for(let i=0;i<iterations;i++)benchmark.run();benchmark.TearDown();}}"
        }
    };
    let tail = ast::tail(program, template)?;
    let first = tail[0].span();
    let work_index = usize::from(source.anchor == WarmAnchor::Typescript);
    let work_span = tail[work_index].span();
    let setup_span = Span::new(0, first.start);
    let mut plan = Plan {
        setup: ast::text(&source.original, setup_span)?.into(),
        saved: String::new(),
        reset: String::new(),
        precheck: String::new(),
        work: ast::text(&source.original, work_span)?.into(),
        check: String::new(),
        cleanup: String::new(),
        final_cleanup: String::new(),
        pieces: vec![
            ast::piece(&source.original, setup_span, WarmPieceRole::Setup)?,
            ast::piece(&source.original, work_span, WarmPieceRole::Work)?,
        ],
        embedded: Vec::new(),
        data: BTreeMap::new(),
        contract: BTreeMap::new(),
        expected: None,
        required: BTreeMap::new(),
    };
    // Preserve trailing source comments rather than discarding them with the driver.
    let suffix = Span::new(
        tail.last().expect("nonempty verified driver").span().end,
        program.span.end,
    );
    plan.setup.push_str(ast::text(&source.original, suffix)?);
    plan.pieces
        .push(ast::piece(&source.original, suffix, WarmPieceRole::Setup)?);
    match source.anchor {
        WarmAnchor::Fib | WarmAnchor::MegaMethod | WarmAnchor::AstCtor => {
            let (name, expected, count) = match source.anchor {
                WarmAnchor::Fib => ("s", 4160200, "5 fib(30) calls"),
                WarmAnchor::MegaMethod => (
                    "total",
                    1396134912,
                    "20 run(1000000); 8 receivers; one method site",
                ),
                _ => (
                    "total",
                    100021998720,
                    "20 run(100000); 24 subclasses; 2000000 constructor chains",
                ),
            };
            if source.anchor == WarmAnchor::AstCtor {
                ast::no_external_driver_var(program, work_span, "r")?;
            }
            let span = ast::result_span(&tail[1])?;
            let result = ast::text(&source.original, span)?;
            plan.reset = format!("{name}=0;");
            plan.check = format!(
                "var value=({result}); if(value!=={expected})throw new Error('original numeric checksum mismatch'); return {{result:{{kind:'integer',value:value}},checks:{{checksum:true}}}};"
            );
            plan.expected = Some(WarmSemanticResult::Integer(expected));
            plan.required
                .insert("checksum".into(), WarmCheckValue::Boolean(true));
            plan.contract.insert("completeWork".into(), count.into());
            plan.pieces
                .push(ast::piece(&source.original, span, WarmPieceRole::Result)?);
        }
        WarmAnchor::Typescript => {
            for name in ["runTypescript", "createCompiler"] {
                integrity_function(&mut plan, source, program, name)?;
            }
            plan.saved = "const __rfWarmInput=compiler_input;".into();
            plan.reset = format!(
                "compiler_input=__rfWarmInput; {}",
                ast::text(&source.original, tail[0].span())?
            );
            plan.precheck = "if(typeof compiler_input!=='string' || compiler_input!==__rfWarmInput || outfile.cumulative_checksum!==0 || outerr.cumulative_checksum!==0)throw new Error('TypeScript input or previous checksum state invalid');".into();
            plan.check = "if((parseErrors.length!==192 && parseErrors.length!==193) || outfile.cumulative_checksum!==0 || outerr.cumulative_checksum!==0)throw new Error('TypeScript outer validation failed');return {result:{kind:'checked-work'},checks:{parseErrorsValid:true,outputChecksumReset:true,parseErrorCount:parseErrors.length}};".into();
            plan.cleanup = ast::text(&source.original, tail[2].span())?.into();
            plan.pieces.push(ast::piece(
                &source.original,
                tail[0].span(),
                WarmPieceRole::ResetSource,
            )?);
            plan.pieces.push(ast::piece(
                &source.original,
                tail[2].span(),
                WarmPieceRole::Cleanup,
            )?);
            plan.expected = Some(WarmSemanticResult::CheckedWork);
            for key in ["parseErrorsValid", "outputChecksumReset"] {
                plan.required
                    .insert(key.into(), WarmCheckValue::Boolean(true));
            }
            plan.contract.insert(
                "completeWork".into(),
                "8 addUnit/reTypeCheck/emit; every original parse-error and Close checksum check"
                    .into(),
            );
            // Inspect the original input initializer as a decoded literal, never reparse its TS work here.
            let mut strings = InputStrings::default();
            strings.visit_program(program);
            let input = strings
                .input
                .ok_or_else(|| reject("missing compiler_input literal"))?;
            plan.data
                .insert("compiler_input".into(), source_sha256(input.as_bytes()));
        }
        WarmAnchor::Crypto => {
            integrity_function(&mut plan, source, program, "encrypt")?;
            integrity_function(&mut plan, source, program, "decrypt")?;
            integrity_function(&mut plan, source, program, "rng_get_byte")?;
            let rng = ast::matching_statement(program, "BenchmarkSuite.ResetRNG();")?;
            let pool = ast::matching_statement(program, CRYPTO_POOL)?;
            plan.reset = format!(
                "rng_state=void 0;rng_pool=void 0;rng_pptr=void 0;encrypted=void 0;\n{}\n{}",
                ast::text(&source.original, rng.span())?,
                ast::text(&source.original, pool.span())?
            );
            plan.precheck = "if(rng_state!=null || rng_pool.length!==256 || rng_pptr!==4)throw new Error('Crypto deterministic pool reset failed');".into();
            let span = ast::result_span(&tail[1])?;
            plan.check = format!(
                "var value=({});if(typeof value!=='string' || value.length===0)throw new Error('Crypto result missing');return {{result:{{kind:'text',value:value}},checks:{{ciphertextPresent:true}}}};",
                ast::text(&source.original, span)?
            );
            for span in [rng.span(), pool.span()] {
                plan.pieces.push(ast::piece(
                    &source.original,
                    span,
                    WarmPieceRole::ResetSource,
                )?);
            }
            plan.pieces
                .push(ast::piece(&source.original, span, WarmPieceRole::Result)?);
            plan.required
                .insert("ciphertextPresent".into(), WarmCheckValue::Boolean(true));
            plan.contract.insert(
                "completeWork".into(),
                "60 full RSA encrypt/decrypt pairs; every original plaintext equality check".into(),
            );
            plan.contract.insert("reset".into(),"original ResetRNG seed49734321 and 256-byte pool initialization with fixed time1122926989487".into());
        }
        WarmAnchor::Zlib => {
            integrity_function(&mut plan, source, program, "runZlib")?;
            let initializer = ast::function(program, "InitializeZlibBenchmark")?;
            let body = initializer
                .body
                .as_ref()
                .ok_or_else(|| reject("zlib initializer has no body"))?;
            let [Statement::ExpressionStatement(statement)] = body.statements.as_slice() else {
                return Err(reject(
                    "zlib initializer must be its sole original literal eval",
                ));
            };
            let Expression::CallExpression(call) = &statement.expression else {
                return Err(reject("zlib initializer is not a call"));
            };
            if !matches!(&call.callee,Expression::Identifier(name) if name.name=="zlibEval")
                || call.arguments.len() != 1
            {
                return Err(reject("zlib initializer is not the original indirect eval"));
            }
            let Argument::StringLiteral(literal) = &call.arguments[0] else {
                return Err(reject("zlib initializer input is not a literal"));
            };
            let decoded = literal.value.to_string();
            let integrity = inspect_zlib(&decoded)?;
            plan.embedded.push(WarmEmbeddedScript {
                container: ast::piece(&source.original, literal.span, WarmPieceRole::Integrity)?,
                decoded_sha256: source_sha256(decoded.as_bytes()),
                integrity,
            });
            plan.saved =
                "InitializeZlibBenchmark();const __rfWarmModule=Module;const __rfWarmYa=Ya;".into();
            plan.reset = "Module.zlibIntegrityComparisons=0;Module.zlibIntegrityBytes=0;Module.zlibIntegrityInputByteSum=0;".into();
            plan.precheck = "if(Module!==__rfWarmModule || Ya!==__rfWarmYa || typeof Ya!=='function')throw new Error('Zlib persistent identity changed');".into();
            let result_span = ast::result_span(&tail[2])?;
            plan.check = format!(
                "{}\nif(Module!==__rfWarmModule || Ya!==__rfWarmYa)throw new Error('Zlib persistent identity changed');return {{result:{{kind:'text',value:({})}},checks:{{comparisons:Module.zlibIntegrityComparisons,bytes:Module.zlibIntegrityBytes,inputByteSum:Module.zlibIntegrityInputByteSum,persistentIdentity:true}}}};",
                ast::text(&source.original, tail[1].span())?,
                ast::text(&source.original, result_span)?
            );
            plan.final_cleanup = ast::text(&source.original, tail[3].span())?.into();
            plan.expected = Some(WarmSemanticResult::Text(
                "zlib-ok:720:72000000:2773014".into(),
            ));
            for (key, value) in [
                ("comparisons", 720),
                ("bytes", 72000000),
                ("inputByteSum", 2773014),
            ] {
                plan.required
                    .insert(key.into(), WarmCheckValue::Integer(value));
            }
            plan.required
                .insert("persistentIdentity".into(), WarmCheckValue::Boolean(true));
            for (span, role) in [
                (tail[1].span(), WarmPieceRole::Integrity),
                (result_span, WarmPieceRole::Result),
                (tail[3].span(), WarmPieceRole::Cleanup),
            ] {
                plan.pieces.push(ast::piece(&source.original, span, role)?);
            }
            plan.contract.insert(
                "completeWork".into(),
                "12 runZlib; 720 full-buffer comparisons; 72000000 bytes; original checksum2773014"
                    .into(),
            );
            plan.contract.insert("lifetime".into(),"original eval initialization once before warmups; original teardown once after all invocations".into());
        }
        WarmAnchor::EarleyBoyer => {
            integrity_function(&mut plan, source, program, "RunBenchmark")?;
            plan.precheck = "if(BenchmarkSuite.suites.length!==1 || BenchmarkSuite.suites[0].name!=='EarleyBoyer' || BenchmarkSuite.suites[0].benchmarks.length!==2 || BenchmarkSuite.suites[0].benchmarks[0].name!=='Earley' || BenchmarkSuite.suites[0].benchmarks[0].deterministicIterations!==2500 || BenchmarkSuite.suites[0].benchmarks[1].name!=='Boyer' || BenchmarkSuite.suites[0].benchmarks[1].deterministicIterations!==200)throw new Error('Earley/Boyer full suite contract changed');".into();
            plan.check = "return {result:{kind:'checked-work'},checks:{fullSuite:true}};".into();
            plan.expected = Some(WarmSemanticResult::CheckedWork);
            plan.required
                .insert("fullSuite".into(), WarmCheckValue::Boolean(true));
            plan.contract.insert(
                "completeWork".into(),
                "2500 Earley + 200 Boyer; original Setup/TearDown and every warn/throw; Earley132"
                    .into(),
            );
        }
    }
    Ok(plan)
}

fn integrity_function(
    plan: &mut Plan,
    source: &WarmSource,
    program: &Program<'_>,
    name: &str,
) -> Result<(), WarmHarnessError> {
    let function = ast::function(program, name)?;
    plan.pieces.push(ast::piece(
        &source.original,
        function.span,
        WarmPieceRole::Integrity,
    )?);
    Ok(())
}

#[derive(Default)]
struct InputStrings {
    input: Option<String>,
}
impl<'a> Visit<'a> for InputStrings {
    fn visit_variable_declarator(&mut self, it: &oxc_ast::ast::VariableDeclarator<'a>) {
        if let oxc_ast::ast::BindingPattern::BindingIdentifier(id) = &it.id
            && id.name == "compiler_input"
            && let Some(Expression::StringLiteral(literal)) = &it.init
        {
            self.input = Some(literal.value.to_string());
        }
        walk::walk_variable_declarator(self, it);
    }
}

const CRYPTO_POOL: &str = "if(rng_pool==null){rng_pool=new Array();rng_pptr=0;var t;while(rng_pptr<rng_psize){t=Math.floor(65536*Math.random());rng_pool[rng_pptr++]=t>>>8;rng_pool[rng_pptr++]=t&255;}rng_pptr=0;rng_seed_time();}";

const MEMCMP: &str = "({_memcmp:function(left,right,count){left=left>>>0;right=right>>>0;count=count>>>0;if(left>Q.length||right>Q.length||count>Q.length-left||count>Q.length-right){throw new Error(\"Zlib memcmp range is invalid\");}var byteSum=0;for(var index=0;index<count;index++){var leftByte=Q[left+index];var difference=leftByte-Q[right+index];if(difference!==0)return difference;byteSum+=leftByte;}if(byteSum!==2773014){throw new Error(\"Zlib input byte checksum mismatch: \"+byteSum);}Module.zlibIntegrityInputByteSum=byteSum;Module.zlibIntegrityComparisons=(Module.zlibIntegrityComparisons||0)+1;Module.zlibIntegrityBytes=(Module.zlibIntegrityBytes||0)+count;return 0;}});";

#[derive(Default)]
struct MemcmpProof {
    spans: Vec<Span>,
    writers: Vec<String>,
}
impl<'a> Visit<'a> for MemcmpProof {
    fn visit_object_property(&mut self, it: &ObjectProperty<'a>) {
        if matches!(&it.key,oxc_ast::ast::PropertyKey::StaticIdentifier(name) if name.name=="_memcmp")
        {
            // Owned source extraction happens after the visitor, while the AST remains alive.
            self.spans.push(it.span);
        }
        walk::walk_object_property(self, it);
    }
    fn visit_assignment_expression(&mut self, it: &oxc_ast::ast::AssignmentExpression<'a>) {
        if let oxc_ast::ast::AssignmentTarget::StaticMemberExpression(member) = &it.left
            && matches!(&member.object,Expression::Identifier(name) if name.name=="Module")
            && member.property.name.starts_with("zlibIntegrity")
        {
            self.writers.push(member.property.name.to_string());
        }
        walk::walk_assignment_expression(self, it);
    }
}

pub(super) fn inspect_zlib(decoded: &str) -> Result<Vec<WarmSourcePiece>, WarmHarnessError> {
    ast::parse(decoded, |program| {
        ast::check_original(program)?;
        let mut proof = MemcmpProof::default();
        proof.visit_program(program);
        if proof.spans.len() != 1 {
            return Err(reject("decoded zlib requires exactly one original _memcmp"));
        }
        proof.writers.sort();
        if proof.writers
            != [
                "zlibIntegrityBytes",
                "zlibIntegrityComparisons",
                "zlibIntegrityInputByteSum",
            ]
        {
            return Err(reject(
                "decoded zlib integrity fields have extra or missing writers",
            ));
        }
        let property = ast::text(decoded, proof.spans[0])?;
        let actual = format!("({{{property}}});");
        let same = ast::parse(&actual, |actual_program| {
            ast::parse(MEMCMP, |expected| Ok(actual_program.content_eq(expected)))
        })?;
        if !same {
            return Err(reject(
                "decoded zlib _memcmp changed range, byte comparison, checksum or counter semantics",
            ));
        }
        Ok(vec![ast::piece(
            decoded,
            proof.spans[0],
            WarmPieceRole::Integrity,
        )?])
    })
}
