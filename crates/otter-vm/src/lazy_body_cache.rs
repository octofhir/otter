//! Lazily compiled bodies kept with a script's code cache.
//!
//! A code cache written when a script first compiles holds only what that
//! compile produced: the top level and the bodies it ran right away. The
//! functions the script calls afterwards compile on their first call, and a
//! page loaded again calls the same ones. For every script an embedder runs
//! from a code cache, this store keeps the encoding of each body compiled on
//! a first call, and answers the same function's first call on a later load
//! from it.
//!
//! # Contents
//! - [`LazyBody`] — one kept body: its function's source range, the
//!   fingerprint of what its compile read, and its encoding.
//! - [`KeptScript`] — a script's code-cache bytes besides its bodies, and
//!   the bodies, as the embedder writes them back.
//! - `LazyBodyStore` — the kept scripts of one isolate, by URL.
//! - `Interpreter::keep_script_cache` / `Interpreter::kept_script_cache` —
//!   the embedder's side.
//!
//! # Invariants
//! - A kept body answers only the function whose source range it was
//!   compiled for, in the script it was kept for, and only under the
//!   fingerprint it was compiled under: the closure's context chain, the
//!   module's top-level facts and its import resolutions. The bytes came
//!   with a code cache keyed by the exact source text and compiler, so the
//!   range names the same function.
//! - A URL two different scripts ran under keeps and answers nothing.
//! - A kept body links as a fresh compile does, through the verifier, and
//!   names the URL it runs under, not the one it was compiled under.
//! - Bodies that are not answered from the store are compiled as before; a
//!   body that does not decode is compiled too.
//!
//! # See also
//! - [`crate::lazy_compile`] for the first-call compile this short-cuts.

use otter_bytecode::{BytecodeModule, EvalCallerChain, LazyEnvironment, ModuleResolution};
use rustc_hash::FxHashMap;

/// One lazily compiled body, as a code cache keeps it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LazyBody {
    /// Source range of the function's lazy record.
    pub range: (u32, u32),
    /// Fingerprint of everything besides the source its compile read.
    pub fingerprint: u64,
    /// The compiled body's module encoding.
    pub bytes: Vec<u8>,
}

/// A script's code cache as the store holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeptScript {
    /// The code-cache bytes the embedder handed in or the first compile
    /// produced, besides the bodies; opaque here.
    pub top: Vec<u8>,
    /// Every body kept for the script, in source order.
    pub bodies: Vec<LazyBody>,
}

#[derive(Debug, Default)]
struct ScriptBodies {
    top: Vec<u8>,
    bodies: FxHashMap<(u32, u32), (u64, Vec<u8>)>,
    /// Bodies compiled since the embedder last took the script.
    compiled: usize,
}

/// What the store holds for one URL.
#[derive(Debug)]
enum Kept {
    Script(ScriptBodies),
    /// Two different scripts ran under the URL: a source range no longer
    /// names one function, so nothing is kept or answered for it.
    Ambiguous,
}

/// The kept scripts of one isolate, by URL.
#[derive(Debug, Default)]
pub(crate) struct LazyBodyStore {
    scripts: FxHashMap<Box<str>, Kept>,
}

impl LazyBodyStore {
    fn script(&self, url: &str) -> Option<&ScriptBodies> {
        match self.scripts.get(url)? {
            Kept::Script(script) => Some(script),
            Kept::Ambiguous => None,
        }
    }

    /// Whether first-call bodies of `url` are kept.
    pub(crate) fn keeps(&self, url: &str) -> bool {
        self.script(url).is_some()
    }

    /// The kept encoding of `range` in `url`, compiled under `fingerprint`.
    pub(crate) fn find(&self, url: &str, range: (u32, u32), fingerprint: u64) -> Option<&[u8]> {
        let (kept, bytes) = self.script(url)?.bodies.get(&range)?;
        (*kept == fingerprint).then_some(bytes.as_slice())
    }

    /// Keep a body `url`'s function at `range` compiled to.
    pub(crate) fn record(&mut self, url: &str, body: LazyBody) {
        if let Some(Kept::Script(script)) = self.scripts.get_mut(url) {
            script
                .bodies
                .insert(body.range, (body.fingerprint, body.bytes));
            script.compiled += 1;
        }
    }
}

/// The fingerprint a body compile of a function in `url` is keyed under.
pub(crate) fn fingerprint(
    chain: &EvalCallerChain,
    environment: Option<&LazyEnvironment>,
    resolutions: &[ModuleResolution],
    url: &str,
) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = rustc_hash::FxHasher::default();
    chain.fingerprint().hash(&mut hasher);
    if let Some(environment) = environment {
        // The environment's own URL is the one the script compiled under;
        // a body names the URL it runs under instead.
        environment.imports.hash(&mut hasher);
        environment.reexports.hash(&mut hasher);
        environment.script_lexicals.hash(&mut hasher);
    }
    for resolution in resolutions.iter().filter(|edge| edge.referrer == url) {
        resolution.hash(&mut hasher);
    }
    hasher.finish()
}

/// The module a kept body encodes, named for `url`.
pub(crate) fn decode(bytes: &[u8], url: &str) -> Option<BytecodeModule> {
    let mut module = otter_bytecode::binary::decode_module_for_link(bytes).ok()?;
    url.clone_into(&mut module.module);
    for function in &mut module.functions {
        url.clone_into(&mut function.module_url);
    }
    Some(module)
}

/// The encoding a body is kept as, unless it is too large to keep.
pub(crate) fn encode(module: &BytecodeModule) -> Option<Vec<u8>> {
    /// A body larger than this compiles again rather than being kept.
    const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;
    otter_bytecode::binary::encode_module_bounded(module, MAX_BODY_BYTES).ok()
}

impl crate::Interpreter {
    /// Keep the bodies `url`'s functions compile to on their first call,
    /// starting from `script`: its bodies answer those calls, and
    /// [`Self::kept_script_cache`] hands them back with every body compiled
    /// meanwhile.
    ///
    /// The same script kept again under its URL keeps what it has; another
    /// script under the same URL ends keeping for that URL, since a source
    /// range no longer names one function there.
    pub fn keep_script_cache(&mut self, url: &str, script: KeptScript) {
        let entry = match self.lazy_body_store.scripts.get(url) {
            None => {
                let bodies = script
                    .bodies
                    .into_iter()
                    .map(|body| (body.range, (body.fingerprint, body.bytes)))
                    .collect();
                Kept::Script(ScriptBodies {
                    top: script.top,
                    bodies,
                    compiled: 0,
                })
            }
            Some(Kept::Script(kept)) if kept.top == script.top => return,
            Some(Kept::Script(_) | Kept::Ambiguous) => Kept::Ambiguous,
        };
        self.lazy_body_store.scripts.insert(url.into(), entry);
    }

    /// `url`'s kept script, when a first call compiled a body since it was
    /// kept or last handed back.
    pub fn kept_script_cache(&mut self, url: &str) -> Option<KeptScript> {
        let Kept::Script(script) = self.lazy_body_store.scripts.get_mut(url)? else {
            return None;
        };
        if script.compiled == 0 {
            return None;
        }
        script.compiled = 0;
        let mut bodies: Vec<LazyBody> = script
            .bodies
            .iter()
            .map(|(&range, (fingerprint, bytes))| LazyBody {
                range,
                fingerprint: *fingerprint,
                bytes: bytes.clone(),
            })
            .collect();
        bodies.sort_unstable_by_key(|body| body.range);
        Some(KeptScript {
            top: script.top.clone(),
            bodies,
        })
    }
}
