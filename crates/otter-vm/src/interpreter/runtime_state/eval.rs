//! `eval_source` — direct/indirect `eval()` re-entry into the source compiler,
//! plus `alloc_syntax_error` for parser-error promotion.

use crate::descriptors::VmNativeCallError;
use crate::value::RegisterValue;

use super::{Interpreter, InterpreterError, RuntimeState};

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

        // §19.2.1.1 Step 4-10: Parse the source as a Script.
        let source_url = if direct {
            "<direct-eval>"
        } else {
            "<indirect-eval>"
        };

        // §B.3.5.2 — If inside a field initializer, apply additional early
        // error rules (ContainsArguments, Contains SuperCall).
        let in_field_init = direct && self.field_initializer_depth > 0;
        let module = if in_field_init {
            crate::source::compile_eval_field_init(source, source_url)
        } else {
            crate::source::compile_eval(source, source_url)
        }
        .map_err(|e| {
            // §19.2.1.1 Step 5: If parsing fails, throw a SyntaxError.
            self.alloc_syntax_error(&format!("eval: {e}"))
        })?;

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

    /// Allocates a SyntaxError object with the given message.
    /// §20.5.5.4 NativeError
    /// Spec: <https://tc39.es/ecma262/#sec-nativeerror-message>
    pub fn alloc_syntax_error(&mut self, message: &str) -> VmNativeCallError {
        let prototype = self.intrinsics().syntax_error_prototype;
        let handle = self.alloc_object_with_prototype(Some(prototype));
        let msg = self.alloc_string(message);
        let msg_prop = self.intern_property_name("message");
        self.objects
            .set_property(handle, msg_prop, RegisterValue::from_object_handle(msg.0))
            .ok();
        let name = self.alloc_string("SyntaxError");
        let name_prop = self.intern_property_name("name");
        self.objects
            .set_property(handle, name_prop, RegisterValue::from_object_handle(name.0))
            .ok();
        VmNativeCallError::Thrown(RegisterValue::from_object_handle(handle.0))
    }
}
