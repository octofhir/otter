//! `eval_source` — direct/indirect `eval()` re-entry into the source compiler,
//! plus `alloc_syntax_error` for parser-error promotion.

use std::hash::{Hash, Hasher};
use std::rc::Rc;

use crate::descriptors::VmNativeCallError;
use crate::interpreter::{EVAL_CACHE_CAPACITY, EvalCacheKey};
use crate::module::Module;
use crate::value::RegisterValue;

use super::{Interpreter, InterpreterError, RuntimeState};

fn hash_source(source: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    source.hash(&mut hasher);
    hasher.finish()
}

/// Compiles `source` as eval code, switching between the ordinary and
/// field-initializer entry points based on `in_field_init`. Pulled out of
/// [`RuntimeState::eval_source`] so the cached and uncached paths share
/// one entry, and the cache key intentionally omits the URL so multiple
/// call-sites of identical source text share one compile.
fn compile_eval(
    source: &str,
    in_field_init: bool,
) -> Result<Module, crate::source::SourceLoweringError> {
    // §19.2.1.1 Step 4-10. The source URL is informational (used in stack
    // traces); the cache key intentionally does NOT include it.
    let source_url = if in_field_init {
        "<eval-field-init>"
    } else {
        "<indirect-eval>"
    };
    if in_field_init {
        crate::source::compile_eval_field_init(source, source_url)
    } else {
        crate::source::compile_eval(source, source_url)
    }
}

impl RuntimeState {
    /// Compiles and executes `source` as a Script in the current runtime.
    /// Returns the completion value of the last expression statement.
    ///
    /// When `direct` is false (indirect eval), the code runs in the global
    /// scope and is never strict unless the eval code itself contains a
    /// "use strict" directive.
    ///
    /// Spec: <https://tc39.es/ecma262/#sec-performeval>
    pub fn eval_source(
        &mut self,
        source: &str,
        direct: bool,
        _strict_caller: bool,
    ) -> Result<RegisterValue, VmNativeCallError> {
        // §19.2.1.1 Step 2: If x is not a String, return x.
        // (Handled by the caller before reaching this method.)

        // §B.3.5.2 — If inside a field initializer, apply additional early
        // error rules (ContainsArguments, Contains SuperCall).
        let in_field_init = direct && self.field_initializer_depth > 0;

        // C5: bytecode cache. Hash the source + the field-init split so
        // tight loops over a templated string skip the parse/lower/codegen
        // pipeline. Direct eval skips the cache because its visible
        // bindings depend on the enclosing closure scope, which the
        // compiled `Module` does not capture.
        //
        // For indirect eval the source bytes alone fully determine the
        // module: there is no enclosing scope leaking into compilation.
        let cache_key = if direct {
            None
        } else {
            Some(EvalCacheKey {
                source_hash: hash_source(source),
                is_field_init: in_field_init,
            })
        };

        let module: Rc<Module> = if let Some(key) = cache_key {
            if let Some(cached) = self.eval_cache_get(&key) {
                cached
            } else {
                let compiled = compile_eval(source, in_field_init).map_err(|e| {
                    self.alloc_syntax_error(&format!("eval: {e}"))
                })?;
                let rc = Rc::new(compiled);
                self.eval_cache_insert(key, Rc::clone(&rc));
                rc
            }
        } else {
            // Direct eval — never cache.
            let compiled = compile_eval(source, in_field_init).map_err(|e| {
                self.alloc_syntax_error(&format!("eval: {e}"))
            })?;
            Rc::new(compiled)
        };

        // §19.2.1.1 Step 16-25: Evaluate the parsed script.
        let interpreter = Interpreter::for_runtime(self);
        let result = interpreter
            .execute_module(&module, self)
            .map_err(|e| match e {
                InterpreterError::UncaughtThrow(value) => VmNativeCallError::Thrown(value),
                other => VmNativeCallError::Internal(format!("eval: {other}").into()),
            })?;

        Ok(result.return_value())
    }

    /// C5 cache lookup. Marks the entry most-recently-used (moves to back).
    /// Returns a cheap `Rc::clone` of the cached module on hit.
    fn eval_cache_get(&mut self, key: &EvalCacheKey) -> Option<Rc<Module>> {
        let position = self.eval_cache.iter().position(|(k, _)| k == key)?;
        // Splice the entry out and push it back so it counts as most recently
        // used. `VecDeque::remove` is O(n) on a 64-element ring; the eval
        // cache is small enough that this stays well below the parse cost.
        let (k, v) = self.eval_cache.remove(position)?;
        let result = Rc::clone(&v);
        self.eval_cache.push_back((k, v));
        Some(result)
    }

    /// C5 cache insert. Evicts the least-recently-used entry once the
    /// capacity is reached.
    fn eval_cache_insert(&mut self, key: EvalCacheKey, module: Rc<Module>) {
        if self.eval_cache.len() == EVAL_CACHE_CAPACITY {
            self.eval_cache.pop_front();
        }
        self.eval_cache.push_back((key, module));
    }

    /// Allocates a SyntaxError object with the given message.
    /// §20.5.5.4 NativeError
    /// Spec: <https://tc39.es/ecma262/#sec-nativeerror-message>
    pub fn alloc_syntax_error(&mut self, message: &str) -> VmNativeCallError {
        let prototype = self.intrinsics().syntax_error_prototype;
        let handle = match self.alloc_object_with_prototype(Some(prototype)) {
            Ok(handle) => handle,
            Err(error) => return VmNativeCallError::from(error),
        };
        // Strategy B: store .message and .name as TAG_PTR_STRING.
        let msg = match self.alloc_string_value(message) {
            Ok(value) => value,
            Err(error) => return VmNativeCallError::from(error),
        };
        let msg_prop = self.intern_property_name("message");
        self.objects.set_property(handle, msg_prop, msg).ok();
        let name = match self.alloc_string_value("SyntaxError") {
            Ok(value) => value,
            Err(error) => return VmNativeCallError::from(error),
        };
        let name_prop = self.intern_property_name("name");
        self.objects.set_property(handle, name_prop, name).ok();
        VmNativeCallError::Thrown(RegisterValue::from_object_handle(handle.0))
    }
}
