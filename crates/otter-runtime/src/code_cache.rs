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
//! - [`Runtime::compile_script_cache`] — produce the bytes without running.
//!
//! # Invariants
//! - Bytes are a magic tag, the compile-cache key of the exact compile
//!   (compiler fingerprint, engine version, source text, kind) and the
//!   module encoding without its source. A classic script's specifier is
//!   not part of the key: its compiled form names the specifier only in the
//!   URLs a run stamps, so the same bytes served from another address (a
//!   bundle whose query string changes every load) are the same compile. A run uses them only when
//!   the key matches the source it is given, and only after the bytecode
//!   verifier accepts the decoded module.
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

use crate::compile_cache::{CompileCacheKey, MAX_COMPILE_CACHE_ENTRY_BYTES, cache_key};
use crate::{ExecutionResult, OtterError, Runtime, SourceInput, module_loader, script_source};

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
            if let Some(verified) = cached {
                let verified = verified.with_module_url(specifier);
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
        encode(&compiled.bytecode, &script_key(source), specifier)
    }
}

/// The key of a classic script's compile: its text and kind, under no
/// specifier, which a run stamps on the decoded functions instead.
fn script_key(source: &SourceInput) -> CompileCacheKey {
    cache_key(&source.text, source.kind, "")
}

/// Code-cache bytes of a fresh compile under `key`.
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
    let mut bytes = Vec::with_capacity(CODE_CACHE_MAGIC.len() + key.as_str().len() + encoded.len());
    bytes.extend_from_slice(CODE_CACHE_MAGIC);
    bytes.extend_from_slice(key.as_str().as_bytes());
    bytes.extend_from_slice(&encoded);
    Ok(bytes)
}

/// The verified module `bytes` hold for exactly this compile, if they hold
/// one.
fn decode(
    bytes: &[u8],
    key: &CompileCacheKey,
    source: &str,
) -> Option<otter_bytecode::VerifiedBytecodeModule> {
    let rest = bytes.strip_prefix(CODE_CACHE_MAGIC.as_slice())?;
    let stored_key = rest.get(..key.as_str().len())?;
    if stored_key != key.as_str().as_bytes() {
        return None;
    }
    let module = &rest[key.as_str().len()..];
    otter_bytecode::binary::decode_module_with_source(module, source).ok()
}
