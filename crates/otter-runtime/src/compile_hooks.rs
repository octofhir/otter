//! Classic script, eval and lazy function compilation.
//!
//! # Contents
//! Classic and lazy compilation, eval-goal source names and CommonJS wrappers.
//!
//! # Invariants
//! Eval retains its caller context and source location.
//!
//! # See also
//! - [`crate::Runtime`] owns the runtime session.

use otter_resource::CompileSource;

use crate::*;

/// Render a compile failure as the message a JS `SyntaxError` carries. The
/// parser's own diagnostics are the message; the error type's `syntax:` prefix
/// belongs in host logs, not in `error.message`.
pub(super) fn compile_error_message(error: otter_compiler::CompileError) -> String {
    match error {
        otter_compiler::CompileError::Syntax { messages, .. } => messages.join("; "),
        other => other.to_string(),
    }
}

/// How a file that is not an explicit module runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CommonJsRouting {
    /// Inside the CommonJS module wrapper.
    CommonJs,
    /// As an ES module: the source parses as one.
    Module,
    /// As a classic script.
    Script,
}

/// The one eval/`new Function`/CommonJS-wrapper compile path every isolate
/// shares. The closure is reusable across calls; each invocation builds a
/// fresh `BytecodeModule`, or admits a builtin's build-produced one.
///
/// A configured compile hook wins over embedded builtin bytecode, exactly as
/// it does for build-produced bootstrap scripts: it receives the identical
/// wrapper text under the builtin's URL.
/// The default compiler's output for the classic script `source`: pure
/// work over the text, which any thread can do.
pub(crate) fn compile_classic_script(
    source: &SourceInput,
    specifier: &str,
) -> Result<CompiledModule, OtterError> {
    if source.allow_top_level_await {
        // Embedder snippet APIs opt into module-grade top-level `await`
        // while staying on the classic-script pipeline.
        let bytecode =
            compile_script_source_with_top_level_await(&source.text, source.kind, specifier)
                .map_err(|err| map_compile_error(err, specifier))?;
        return CompiledModule::from_bytecode(bytecode)
            .map_err(|err| map_compile_error(err, specifier));
    }
    otter_compiler::compile_script_source_to_module_with_options(
        &source.text,
        source.kind,
        specifier,
        LAZY_FUNCTIONS,
    )
    .map_err(|err| map_compile_error(err, specifier))
}

pub(super) fn standard_eval_hook(
    compile_hook: Option<std::sync::Arc<dyn RuntimeCompileHook>>,
    code_cache: hooks::CodeCacheSlot,
) -> otter_vm::EvalHook {
    std::sync::Arc::new(
        move |input: CompileSource<'_>, options: EvalCompileOptions| {
            let source = input.text();
            if let Some(unit) = options.embedded {
                return embedded_commonjs_module(unit, compile_hook.as_deref());
            }
            if options.commonjs_file && compile_hook.is_none() {
                return commonjs_file_module(source, &options);
            }
            // §16.1.6 ScriptEvaluation — host-requested script
            // execution ($262.evalScript) compiles under script
            // GDI semantics, not eval semantics.
            if options.script_goal {
                return otter_compiler::compile_script_source(
                    source,
                    SourceKind::JavaScript,
                    "<evalScript>",
                )
                .map(otter_vm::CompiledEvalSource::Fresh)
                .map_err(compile_error_message);
            }
            if compile_hook.is_none()
                && code_cache::eval_is_cacheable(source, &options)
                && let Some(cache) = code_cache.get()
            {
                return code_cache::cached_eval(&*cache, source, &options, || {
                    compile_eval_with_options(input, &options).map_err(compile_error_message)
                });
            }
            let module =
                compile_eval_with_options(input, &options).map_err(compile_error_message)?;
            Ok(otter_vm::CompiledEvalSource::Fresh(module))
        },
    )
}

/// A builtin CommonJS wrapper the product build compiled: the configured
/// compiler's output for its exact text when a compile hook is installed,
/// otherwise the embedded module after full verification.
pub(super) fn embedded_commonjs_module(
    unit: &'static EmbeddedCommonJs,
    compile_hook: Option<&dyn RuntimeCompileHook>,
) -> Result<otter_vm::CompiledEvalSource, String> {
    if let Some(hook) = compile_hook {
        let source = module_loader::ResolvedSource {
            url: unit.url.to_owned(),
            kind: SourceKind::JavaScript,
            jsx: None,
            text: otter_resource::SharedSource::from_static(unit.source),
        };
        return hook
            .compile(RuntimeCompileRequest { source: &source })
            .map(|mut compiled| {
                compiled.bytecode.mark_primordial_iteration();
                otter_vm::CompiledEvalSource::Fresh(compiled.bytecode)
            })
            .map_err(|error| error.to_string());
    }
    // The build verified these exact bytes; they are part of the binary.
    otter_bytecode::binary::decode_build_artifact(unit.bytecode)
        .map(otter_vm::CompiledEvalSource::Verified)
        .map_err(|error| format!("invalid embedded bytecode for '{}': {error}", unit.url))
}

/// A file module's CommonJS wrapper: a compile kept from an earlier run of
/// the same text when the compile cache holds one, else a fresh compile that
/// is kept for the next run. Package code is loaded again on every launch, so
/// unlike one-off scripts its entries are read back.
pub(super) fn commonjs_file_module(
    source: &str,
    options: &EvalCompileOptions,
) -> Result<otter_vm::CompiledEvalSource, String> {
    let cache = compile_cache::CompileCache::user_default();
    let key = cache
        .as_ref()
        .map(|_| compile_cache::cache_key(source, SourceKind::JavaScript, "<commonjs>"));
    if let (Some(cache), Some(key)) = (&cache, &key)
        && let Some(verified) = cache.load(key, source)
    {
        return Ok(otter_vm::CompiledEvalSource::Verified(verified));
    }
    let module = compile_eval_with_options(CompileSource::Borrowed(source), options)
        .map_err(compile_error_message)?;
    if let (Some(cache), Some(key)) = (&cache, &key) {
        cache.store(key, &module);
    }
    Ok(otter_vm::CompiledEvalSource::Fresh(module))
}

/// Compile eval-goal `source` the way the VM's compile requests ask for it.
/// §19.2.1.3 — a direct eval compiles against its call site's context chain;
/// indirect eval, `Function`, and the CommonJS wrapper carry none.
fn compile_eval_with_options(
    input: CompileSource<'_>,
    options: &EvalCompileOptions,
) -> Result<otter_bytecode::BytecodeModule, otter_compiler::CompileError> {
    let specifier = source_url_comment(input.text()).unwrap_or("<eval>");
    let policy = if options.commonjs_file {
        compile_cache::COMMONJS_FUNCTIONS
    } else {
        LAZY_FUNCTIONS
    };
    otter_compiler::compile_eval_source(
        input,
        SourceKind::JavaScript,
        specifier,
        options.force_strict,
        options.forbid_var_arguments,
        options.caller_chain.as_ref(),
        options.new_target_allowed,
        options.in_class_field_initializer,
        options.super_property_allowed,
        options.super_call_allowed,
        options.dynamic_parameters_end,
        policy,
    )
}

/// The name eval-goal code gives itself with a `//# sourceURL=` comment (or
/// the older `//@ sourceURL=`), the last one when there are several.
///
/// Bundlers and script loaders that run fetched text through `eval` name it
/// this way, so its functions are reported — in stacks and to a dynamic
/// `import()` resolving a relative specifier — as the file it came from rather
/// than as anonymous eval code. Only a name with no whitespace in it counts.
fn source_url_comment(source: &str) -> Option<&str> {
    const SUFFIX: &str = " sourceURL=";
    let bytes = source.as_bytes();
    let marker = memchr::memrchr2_iter(b'#', b'@', bytes).find(|&at| {
        bytes[..at].ends_with(b"//") && bytes[at + 1..].starts_with(SUFFIX.as_bytes())
    })?;
    let rest = &source[marker + 1 + SUFFIX.len()..];
    let end = rest
        .find(['\n', '\r', '\u{2028}', '\u{2029}'])
        .unwrap_or(rest.len());
    let name = rest[..end].trim();
    (!name.is_empty() && !name.contains(char::is_whitespace) && !name.contains(['"', '\'']))
        .then_some(name)
}

/// The compiler of lazily compiled function bodies.
pub(crate) fn lazy_compile_hook() -> otter_vm::LazyCompileHook {
    std::sync::Arc::new(|input: otter_vm::LazyCompileInput<'_>| {
        otter_compiler::compile_lazy_function(otter_compiler::LazyCompileRequest {
            source: input.source,
            module_url: input.module_url,
            function: input.function,
            chain: input.chain,
            environment: input.environment,
            resolutions: input.resolutions,
        })
        .map_err(|error| match error {
            otter_compiler::CompileError::Syntax { messages, .. } => messages.join("; "),
            other => other.to_string(),
        })
    })
}
