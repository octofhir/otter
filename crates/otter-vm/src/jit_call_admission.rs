//! Semantic admission for shared generated call linkage.
//!
//! # Contents
//! - One CodeBlock policy for ordinary, guarded-method and constructor targets.
//!
//! # Invariants
//! - Admission describes function semantics, not generation liveness or callable
//!   identity. Native entry selection and generated guards prove those separately.
//! - Ordinary calls can consume complete actual windows; guarded methods retain
//!   their current narrower argument contract. Constructors never admit methods.
//! - Rest parameters consume their own published actual window; they do not
//!   prevent plain or constructor linkage, but their body cannot be spliced.
//! - Dynamic target selection and compile-time baking must use this same policy.
//!
//! # See also
//! - [`crate::jit_registry`] — current native generation and frame metadata.
//! - [`crate::interp::jit_compile`] — source-site planning and inline candidates.

use crate::{CodeBlock, jit::JitDirectCallKind};

impl CodeBlock {
    pub(crate) fn admits_generated_call(&self, kind: JitDirectCallKind) -> bool {
        if self.is_generator
            || self.is_async
            || self.is_async_generator
            || self.contains_direct_eval
        {
            return false;
        }
        match kind {
            JitDirectCallKind::Plain => !self.is_derived_constructor,
            JitDirectCallKind::Method => {
                !self.is_derived_constructor && !self.needs_arguments && !self.has_rest
            }
            JitDirectCallKind::Construct
            | JitDirectCallKind::DerivedConstruct
            | JitDirectCallKind::SuperConstruct
            | JitDirectCallKind::DerivedSuperConstruct => {
                !self.is_method && !(self.is_derived_constructor && self.makes_function)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use otter_bytecode::{Function, FunctionCodeBuilder, Op, Operand};

    fn rest_function(mut function: Function) -> crate::executable::ExecutableModule {
        let mut code = FunctionCodeBuilder::new();
        code.push(Op::CollectRest, &[Operand::Register(1)]);
        code.push(Op::ReturnValue, &[Operand::Register(1)]);
        function.has_rest = true;
        function.param_count = 1;
        function.locals = 2;
        function.code = code.finish();
        let mut module = crate::test_support::minimal_bytecode_module("rest-call-admission.js");
        module.functions[0] = function;
        crate::executable::ExecutableModule::from_bytecode(&module)
    }

    #[test]
    fn rest_body_without_arguments_uses_own_window_and_admits_plain_and_construct_linkage() {
        let module = rest_function(Function::default());
        let function = module.function(0).unwrap();
        assert!(function.has_rest);
        assert!(!function.needs_arguments);
        assert!(function.requires_argument_frame());
        assert!(function.admits_generated_call(JitDirectCallKind::Plain));
        assert!(function.admits_generated_call(JitDirectCallKind::Construct));
        assert!(!function.admits_generated_call(JitDirectCallKind::Method));
        assert_ne!(
            function.call_flags() & crate::native_abi::FUNCTION_CALL_CONSTRUCTIBLE,
            0
        );
    }

    #[test]
    fn rest_linkage_keeps_suspension_eval_and_constructor_semantic_refusals() {
        for function in [
            Function {
                is_generator: true,
                ..Function::default()
            },
            Function {
                is_async: true,
                ..Function::default()
            },
            Function {
                is_async_generator: true,
                ..Function::default()
            },
            Function {
                contains_direct_eval: true,
                ..Function::default()
            },
        ] {
            let module = rest_function(function);
            let function = module.function(0).unwrap();
            assert!(function.requires_argument_frame());
            for kind in [
                JitDirectCallKind::Plain,
                JitDirectCallKind::Method,
                JitDirectCallKind::Construct,
            ] {
                assert!(!function.admits_generated_call(kind));
            }
        }

        let module = rest_function(Function {
            is_method: true,
            ..Function::default()
        });
        let function = module.function(0).unwrap();
        assert!(function.admits_generated_call(JitDirectCallKind::Plain));
        assert!(!function.admits_generated_call(JitDirectCallKind::Method));
        assert!(!function.admits_generated_call(JitDirectCallKind::Construct));

        let module = rest_function(Function {
            is_derived_constructor: true,
            ..Function::default()
        });
        let function = module.function(0).unwrap();
        assert!(!function.admits_generated_call(JitDirectCallKind::Plain));
        assert!(!function.admits_generated_call(JitDirectCallKind::Method));
        assert!(function.admits_generated_call(JitDirectCallKind::DerivedConstruct));
        assert!(function.admits_generated_call(JitDirectCallKind::DerivedSuperConstruct));
    }
}
