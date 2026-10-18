//! Code cache an embedder keeps beside a script it loads again.
//!
//! A browser loading a page it saw before has every script's bytes in its
//! disk cache. It can keep the compiled script beside them: the first run
//! hands back bytes, the next run takes them and skips parsing and
//! compiling. The bytes are this engine's to read; nobody else interprets
//! them.
//!
//! # Contents
//! - [`Runtime::run_script_cached`] — run a classic script from cached
//!   bytes when they still match, and produce fresh ones when they do not.
//! - [`PreparedScript`] / [`Runtime::run_prepared_script`] — the same in two
//!   steps: compile or decode on any thread, then run.
//! - [`Runtime::script_code_cache`] — the bytes again once the script's
//!   functions have run, with the bodies they compiled on their first call.
//! - [`Runtime::web_module_code_caches`] — the same for delivered web
//!   modules, whose bytes the module map reads back.
//! - [`Runtime::compile_script_cache`] — produce the bytes without running.
//!
//! # Invariants
//! - Bytes are a magic tag, the compile-cache key of the exact compile
//!   (compiler fingerprint, engine version, source text, kind), the module
//!   encoding without its source, and a section of function bodies compiled
//!   on a first call (`otter_vm::LazyBody`). A run uses them only when the
//!   key matches the source it is given, and only after the bytecode
//!   verifier accepts the decoded module; a body links through the verifier
//!   when its function is first called.
//! - A classic script's specifier is not part of the key: its compiled form
//!   names the specifier only in the URLs a run stamps, so the same bytes
//!   served from another address (a bundle whose query string changes every
//!   load) are the same compile.
//! - Anything that does not match — another engine build, an edited script,
//!   a damaged entry — is a miss: the script compiles as it would without a
//!   cache, and fresh bytes come back.
//! - A cached run is the run the same compile would give: function URLs are
//!   stamped with the specifier exactly as a fresh compile's are.
//! - The bytes hold the default compiler's output; a runtime with a compile
//!   hook neither uses nor produces them.
//!
//! # See also
//! - `crate::compile_cache` for the key and the engine's own disk cache.
//! - `otter_vm::lazy_body_cache` for how first-call bodies are kept and used.

use crate::compile_cache::{CompileCacheKey, MAX_COMPILE_CACHE_ENTRY_BYTES, cache_key};
use crate::{
    ExecutionResult, OtterError, Runtime, RuntimeCodeCacheHook, SourceInput, module_loader,
    script_source,
};
use otter_vm::{KeptScript, LazyBody};

/// First bytes of every code-cache entry.
const CODE_CACHE_MAGIC: &[u8; 8] = b"ottercc\0";

/// First bytes of every entry for text compiled by `eval`.
const EVAL_CACHE_MAGIC: &[u8; 8] = b"otterev\0";

/// Shortest `eval` text worth a cache entry: below it, compiling again
/// costs less than reading back.
const MIN_CACHED_EVAL_BYTES: usize = 16 * 1024;

impl Runtime {
    /// Run a classic script, compiling it only when `cache` does not hold
    /// its compiled form.
    ///
    /// `cache` is what an earlier call returned for the same script, or
    /// `None` the first time. Returns the run's result and, when the cache
    /// could not be used, fresh bytes to keep in its place.
    ///
    /// # Errors
    /// As [`Self::run_script`].
    pub fn run_script_cached(
        &mut self,
        source: SourceInput,
        specifier: &str,
        cache: Option<&[u8]>,
    ) -> Result<(ExecutionResult, Option<Vec<u8>>), OtterError> {
        // A compile hook is another compiler: its output is not what the
        // key names, so the cache neither serves nor records it.
        if self.config.hooks.compile_hook().is_some() {
            return self
                .run_script(source, specifier)
                .map(|result| (result, None));
        }
        self.run_prepared_script(PreparedScript::prepare(source, specifier, cache))
    }

    /// Run a script [`PreparedScript::prepare`] compiled or read back,
    /// possibly on another thread: the same run [`Self::run_script_cached`]
    /// gives, and the same fresh bytes.
    ///
    /// # Errors
    /// As [`Self::run_script`], the prepared script's syntax errors
    /// included.
    pub fn run_prepared_script(
        &mut self,
        prepared: PreparedScript,
    ) -> Result<(ExecutionResult, Option<Vec<u8>>), OtterError> {
        let PreparedScript {
            source,
            specifier,
            prepared,
        } = prepared;
        // A compile hook is another compiler, whose output the prepared
        // script is not.
        if self.config.hooks.compile_hook().is_some() {
            return self
                .run_script(source, &specifier)
                .map(|result| (result, None));
        }
        let specifier = specifier.as_str();
        self.interp.begin_jit_debug_capture();
        let (result, fresh) = self.with_direct_timeout(move |runtime| {
            let start = std::time::Instant::now();
            match prepared? {
                Prepared::Cached(Decoded { module, kept }) => {
                    runtime.interp.keep_script_cache(specifier, kept);
                    let verified = module.with_module_url(specifier);
                    let text = module_loader::admit_source(
                        &runtime.config.resource_account,
                        specifier,
                        source.text,
                    )
                    .map_err(module_loader::LoaderError::into_otter_error)?;
                    let sources = script_source::script_sources(
                        verified.module(),
                        text,
                        specifier,
                        &runtime.config.resource_account,
                    )?;
                    let context = runtime.interp.link_verified_module(verified, sources)?;
                    runtime
                        .run_linked_script_with_context_since(context, start)
                        .map(|(result, _)| (result, None))
                }
                Prepared::Fresh { compiled, top } => {
                    runtime.note_script_compiled(&compiled);
                    runtime.interp.keep_script_cache(
                        specifier,
                        KeptScript {
                            top: top.clone(),
                            bodies: Vec::new(),
                        },
                    );
                    let bytes = with_bodies(top, &[]);
                    let (module, sources) =
                        runtime.prepare_script_source(compiled.bytecode, source, specifier)?;
                    runtime
                        .run_compiled_script_with_context_since(module, sources, start)
                        .map(|(result, _)| (result, Some(bytes)))
                }
            }
        })?;
        Ok((self.attach_jit_debug_report(result), fresh))
    }

    /// Compile a classic script into the bytes [`Self::run_script_cached`]
    /// runs it from, without running it.
    ///
    /// # Errors
    /// The script's syntax errors, or a module too large to encode.
    pub fn compile_script_cache(
        &self,
        source: &SourceInput,
        specifier: &str,
    ) -> Result<Vec<u8>, OtterError> {
        let compiled = self.compile_source(source, specifier)?;
        encode(&compiled.bytecode, &script_key(source), specifier).map(|top| with_bodies(top, &[]))
    }

    /// The bytes to keep for `specifier` in place of the ones
    /// [`Self::run_script_cached`] ran it from or returned, now that its
    /// functions have run: the same script, and every function body
    /// compiled on a first call since, so a later run compiles none of
    /// them. `None` when no body compiled since the script ran or since the
    /// last call, so the kept bytes are still the whole answer.
    pub fn script_code_cache(&mut self, specifier: &str) -> Option<Vec<u8>> {
        let kept = self.interp.kept_script_cache(specifier)?;
        Some(with_bodies(kept.top, &kept.bodies))
    }

    /// The code caches to keep for delivered web modules (see
    /// [`crate::web_modules::WebModuleMap::produce_code_caches`]): each module compiled
    /// since the last call, and each module whose functions compiled a
    /// body on a first call since, with every body it kept.
    pub fn web_module_code_caches(&mut self) -> Vec<crate::web_modules::WebModuleCodeCache> {
        let Some(map) = self.web_module_map().cloned() else {
            return Vec::new();
        };
        let mut caches = map.take_code_caches();
        for (compiled_as, request_url) in map.kept_urls() {
            if let Some(kept) = self.interp.kept_script_cache(&compiled_as) {
                caches.retain(|cache| cache.request_url != request_url);
                caches.push(crate::web_modules::WebModuleCodeCache {
                    request_url,
                    bytes: with_bodies(kept.top, &kept.bodies),
                });
            }
        }
        caches
    }

    /// Keep the compiled form of large `eval` text in `hook`, and
    /// read it back from there: a page that evaluates the same library on
    /// every load (a script fetched and handed to `eval`) compiles it once.
    pub fn set_code_cache_hook(&self, hook: Option<std::sync::Arc<dyn RuntimeCodeCacheHook>>) {
        self.code_cache_hook.set(hook);
    }

    /// Store again, through the code-cache hook, each cached `eval` whose
    /// functions compiled a body on a first call since it ran or since the
    /// last call, with every body it kept.
    pub fn keep_eval_code_caches(&mut self) {
        let Some(hook) = self.code_cache_hook.get() else {
            return;
        };
        for (_, kept) in self
            .interp
            .kept_script_caches(|top| top.starts_with(EVAL_CACHE_MAGIC))
        {
            let Some(key) = eval_entry_key(&kept.top) else {
                continue;
            };
            let key = key.to_owned();
            hook.store(&key, with_bodies(kept.top, &kept.bodies));
        }
    }

    /// Hand the interpreter the web modules whose first-call bodies it is
    /// to keep. Runs before a module graph evaluates, so every first call
    /// of the graph's functions finds them.
    pub(crate) fn adopt_web_module_bodies(&mut self) {
        let Some(map) = self.web_module_map().cloned() else {
            return;
        };
        for (compiled_as, kept) in map.take_kept() {
            self.interp.keep_script_cache(&compiled_as, kept);
        }
    }
}

/// The key of a classic script's compile: its text and kind, under no
/// specifier, which a run stamps on the decoded functions instead.
fn script_key(source: &SourceInput) -> CompileCacheKey {
    cache_key(&source.text, source.kind, "")
}

/// A classic script compiled, or read back from its code-cache bytes, by
/// [`Self::prepare`] on any thread, for [`Runtime::run_prepared_script`]
/// to run on the isolate's.
///
/// A browser has a script's bytes before it runs the script: preparing it
/// as they arrive takes the parse and compile (or the decode and verify)
/// off the thread that runs the page.
pub struct PreparedScript {
    source: SourceInput,
    specifier: String,
    prepared: Result<Prepared, OtterError>,
}

enum Prepared {
    /// The code cache held this compile.
    Cached(Decoded),
    /// A fresh compile, and its code-cache bytes besides the body section.
    Fresh {
        compiled: otter_compiler::CompiledModule,
        top: Vec<u8>,
    },
}

const _: fn() = || {
    fn send<T: Send>() {}
    send::<PreparedScript>();
};

impl PreparedScript {
    /// Compile `source`, run as `specifier`, unless `cache` holds its
    /// compiled form, which is then decoded and verified instead.
    #[must_use]
    pub fn prepare(source: SourceInput, specifier: &str, cache: Option<&[u8]>) -> Self {
        let key = script_key(&source);
        let prepared = match cache.and_then(|bytes| decode(bytes, &key, &source.text)) {
            Some(decoded) => Ok(Prepared::Cached(decoded)),
            None => crate::compile_classic_script(&source, specifier).and_then(|compiled| {
                let top = encode(&compiled.bytecode, &key, specifier)?;
                Ok(Prepared::Fresh { compiled, top })
            }),
        };
        Self {
            source,
            specifier: specifier.to_owned(),
            prepared,
        }
    }

    /// The specifier the script runs as.
    #[must_use]
    pub fn specifier(&self) -> &str {
        &self.specifier
    }
}

/// Whether `eval` of `source` under `options` is kept in the code cache: a
/// large text a page evaluates, directly or not, as eval code.
pub(crate) fn eval_is_cacheable(source: &str, options: &otter_vm::EvalCompileOptions) -> bool {
    source.len() >= MIN_CACHED_EVAL_BYTES
        && options.embedded.is_none()
        && !options.commonjs_file
        && !options.script_goal
        && !options.function_constructor
}

/// The key a cached `eval` of `source` under `options` is stored under: the
/// text, and everything else its compile reads — the option flags and, for
/// a direct eval, the fingerprint of the caller's context chain.
fn eval_key(source: &str, options: &otter_vm::EvalCompileOptions) -> CompileCacheKey {
    let flags = [
        options.force_strict,
        options.forbid_var_arguments,
        options.new_target_allowed,
        options.in_class_field_initializer,
        options.super_property_allowed,
        options.super_call_allowed,
    ]
    .map(|flag| if flag { '1' } else { '0' })
    .iter()
    .collect::<String>();
    let chain = options.caller_chain.as_ref().map_or_else(
        || "indirect".to_owned(),
        |chain| format!("{:016x}", chain.fingerprint()),
    );
    cache_key(
        source,
        crate::SourceKind::JavaScript,
        &format!("<eval:{flags}:{chain}>"),
    )
}

/// The hook key of an eval entry whose bytes besides the bodies are `top`.
fn eval_entry_key(top: &[u8]) -> Option<&str> {
    let rest = top.strip_prefix(EVAL_CACHE_MAGIC.as_slice())?;
    let key = rest.get(..CompileCacheKey::LEN)?;
    std::str::from_utf8(key).ok()
}

/// An `eval` of `source`, read back from `hook` when it holds this
/// compile, compiled by `compile` and stored there when it does not. Either
/// way the interpreter keeps the bodies its functions compile on a first
/// call, for [`Runtime::keep_eval_code_caches`].
pub(crate) fn cached_eval(
    hook: &dyn RuntimeCodeCacheHook,
    source: &str,
    options: &otter_vm::EvalCompileOptions,
    compile: impl FnOnce() -> Result<otter_bytecode::BytecodeModule, String>,
) -> Result<otter_vm::CompiledEvalSource, String> {
    let key = eval_key(source, options);
    if let Some(bytes) = hook.load(key.as_str())
        && let Some((module, kept)) = decode_eval(&bytes, &key, source)
    {
        return Ok(otter_vm::CompiledEvalSource::Kept { module, kept });
    }
    let module = compile()?;
    let Ok(top) = encode_with(EVAL_CACHE_MAGIC, &module, &key, "<eval>") else {
        return Ok(otter_vm::CompiledEvalSource::Fresh(module));
    };
    hook.store(key.as_str(), with_bodies(top.clone(), &[]));
    Ok(otter_vm::CompiledEvalSource::Kept {
        module,
        kept: KeptScript {
            top,
            bodies: Vec::new(),
        },
    })
}

/// The unverified module and bodies of an eval entry for exactly `source`;
/// the interpreter verifies the module when it links it.
fn decode_eval(
    bytes: &[u8],
    key: &CompileCacheKey,
    source: &str,
) -> Option<(otter_bytecode::BytecodeModule, KeptScript)> {
    let mut rest = bytes.strip_prefix(EVAL_CACHE_MAGIC.as_slice())?;
    rest = rest.strip_prefix(key.as_str().as_bytes())?;
    let length = take_u32(&mut rest)? as usize;
    let module = rest.get(..length)?;
    rest = &rest[length..];
    let top = bytes[..bytes.len() - rest.len()].to_vec();
    let bodies = read_bodies(rest)?;
    let module = otter_bytecode::binary::decode_module_for_link(module, Some(source)).ok()?;
    Some((module, KeptScript { top, bodies }))
}

/// Code-cache bytes of a fresh compile under `key`, without the body
/// section [`with_bodies`] appends.
fn encode(
    module: &otter_bytecode::BytecodeModule,
    key: &CompileCacheKey,
    specifier: &str,
) -> Result<Vec<u8>, OtterError> {
    encode_with(CODE_CACHE_MAGIC, module, key, specifier)
}

/// [`encode`] under the entry tag `magic`.
fn encode_with(
    magic: &[u8; 8],
    module: &otter_bytecode::BytecodeModule,
    key: &CompileCacheKey,
    specifier: &str,
) -> Result<Vec<u8>, OtterError> {
    let encoded = otter_bytecode::binary::encode_module_bounded_detached(
        module,
        MAX_COMPILE_CACHE_ENTRY_BYTES,
    )
    .map_err(|error| OtterError::Internal {
        code: crate::DiagnosticCode::CompileUnknown.as_str().to_string(),
        message: format!("{specifier}: the compiled script cannot be cached: {error:?}"),
    })?;
    let length = u32::try_from(encoded.len()).map_err(|_| OtterError::Internal {
        code: crate::DiagnosticCode::CompileUnknown.as_str().to_string(),
        message: format!("{specifier}: the compiled script is too large to cache"),
    })?;
    let mut bytes = Vec::with_capacity(magic.len() + key.as_str().len() + 4 + encoded.len());
    bytes.extend_from_slice(magic);
    bytes.extend_from_slice(key.as_str().as_bytes());
    bytes.extend_from_slice(&length.to_le_bytes());
    bytes.extend_from_slice(&encoded);
    Ok(bytes)
}

/// `top` followed by the section of first-call `bodies`: their count, then
/// each one's source range, fingerprint, length and encoding.
pub(crate) fn with_bodies(mut top: Vec<u8>, bodies: &[LazyBody]) -> Vec<u8> {
    let kept: Vec<&LazyBody> = bodies
        .iter()
        .filter(|body| u32::try_from(body.bytes.len()).is_ok())
        .collect();
    top.reserve(4 + kept.iter().map(|body| 20 + body.bytes.len()).sum::<usize>());
    top.extend_from_slice(&(kept.len() as u32).to_le_bytes());
    for body in kept {
        top.extend_from_slice(&body.range.0.to_le_bytes());
        top.extend_from_slice(&body.range.1.to_le_bytes());
        top.extend_from_slice(&body.fingerprint.to_le_bytes());
        top.extend_from_slice(&(body.bytes.len() as u32).to_le_bytes());
        top.extend_from_slice(&body.bytes);
    }
    top
}

/// What a code cache held for exactly this compile.
struct Decoded {
    /// The script's verified module.
    module: otter_bytecode::VerifiedBytecodeModule,
    /// Its bytes besides the bodies, and the bodies, for the interpreter to
    /// answer first calls from and hand back.
    kept: KeptScript,
}

/// The module and bodies `bytes` hold for exactly this compile, if they
/// hold them.
fn decode(bytes: &[u8], key: &CompileCacheKey, source: &str) -> Option<Decoded> {
    let mut rest = bytes.strip_prefix(CODE_CACHE_MAGIC.as_slice())?;
    rest = rest.strip_prefix(key.as_str().as_bytes())?;
    let length = take_u32(&mut rest)? as usize;
    let module = rest.get(..length)?;
    rest = &rest[length..];
    let top = bytes[..bytes.len() - rest.len()].to_vec();
    let bodies = read_bodies(rest)?;
    let module = otter_bytecode::binary::decode_module_with_source(module, source).ok()?;
    Some(Decoded {
        module,
        kept: KeptScript { top, bodies },
    })
}

/// The body section [`with_bodies`] wrote, which must be all of `rest`.
pub(crate) fn read_bodies(mut rest: &[u8]) -> Option<Vec<LazyBody>> {
    let count = take_u32(&mut rest)?;
    let mut bodies = Vec::with_capacity(count.min(1 << 16) as usize);
    for _ in 0..count {
        let start = take_u32(&mut rest)?;
        let end = take_u32(&mut rest)?;
        let fingerprint = u64::from_le_bytes(rest.get(..8)?.try_into().ok()?);
        rest = &rest[8..];
        let length = take_u32(&mut rest)? as usize;
        bodies.push(LazyBody {
            range: (start, end),
            fingerprint,
            bytes: rest.get(..length)?.to_vec(),
        });
        rest = &rest[length..];
    }
    rest.is_empty().then_some(bodies)
}

pub(crate) fn take_u32(rest: &mut &[u8]) -> Option<u32> {
    let value = u32::from_le_bytes(rest.get(..4)?.try_into().ok()?);
    *rest = &rest[4..];
    Some(value)
}
