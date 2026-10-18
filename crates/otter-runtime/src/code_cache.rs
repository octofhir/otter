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
//! - [`Runtime::script_code_cache`] — the bytes again once the script's
//!   functions have run, with the bodies they compiled on their first call.
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
use crate::{ExecutionResult, OtterError, Runtime, SourceInput, module_loader, script_source};
use otter_vm::{KeptScript, LazyBody};

/// First bytes of every code-cache entry.
const CODE_CACHE_MAGIC: &[u8; 8] = b"ottercc\0";

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
        let key = script_key(&source);
        let cached = cache.and_then(|bytes| decode(bytes, &key, &source.text));
        self.interp.begin_jit_debug_capture();
        let (result, fresh) = self.with_direct_timeout(move |runtime| {
            let start = std::time::Instant::now();
            if let Some(Decoded { module, kept }) = cached {
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
                return runtime
                    .run_linked_script_with_context_since(context, start)
                    .map(|(result, _)| (result, None));
            }
            let compiled = runtime.compile_source(&source, specifier)?;
            let bytes = encode(&compiled.bytecode, &key, specifier)?;
            runtime.interp.keep_script_cache(
                specifier,
                KeptScript {
                    top: bytes.clone(),
                    bodies: Vec::new(),
                },
            );
            let bytes = with_bodies(bytes, &[]);
            let (module, sources) =
                runtime.prepare_script_source(compiled.bytecode, source, specifier)?;
            runtime
                .run_compiled_script_with_context_since(module, sources, start)
                .map(|(result, _)| (result, Some(bytes)))
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
}

/// The key of a classic script's compile: its text and kind, under no
/// specifier, which a run stamps on the decoded functions instead.
fn script_key(source: &SourceInput) -> CompileCacheKey {
    cache_key(&source.text, source.kind, "")
}

/// Code-cache bytes of a fresh compile under `key`, without the body
/// section [`with_bodies`] appends.
fn encode(
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
    let mut bytes =
        Vec::with_capacity(CODE_CACHE_MAGIC.len() + key.as_str().len() + 4 + encoded.len());
    bytes.extend_from_slice(CODE_CACHE_MAGIC);
    bytes.extend_from_slice(key.as_str().as_bytes());
    bytes.extend_from_slice(&length.to_le_bytes());
    bytes.extend_from_slice(&encoded);
    Ok(bytes)
}

/// `top` followed by the section of first-call `bodies`: their count, then
/// each one's source range, fingerprint, length and encoding.
fn with_bodies(mut top: Vec<u8>, bodies: &[LazyBody]) -> Vec<u8> {
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
    if !rest.is_empty() {
        return None;
    }
    let module = otter_bytecode::binary::decode_module_with_source(module, source).ok()?;
    Some(Decoded {
        module,
        kept: KeptScript { top, bodies },
    })
}

fn take_u32(rest: &mut &[u8]) -> Option<u32> {
    let value = u32::from_le_bytes(rest.get(..4)?.try_into().ok()?);
    *rest = &rest[4..];
    Some(value)
}
