//! Scalar reads of an activation's implicit arguments object.
//!
//! # Contents
//! - Interpreter and native entry points share argument-window reads.
//! - An observable property access materializes one stable object identity.
//!
//! # Invariants
//! - Only a compiler-proven non-escaping binding may use these operations.
//! - In-range actuals are own data properties and require no prototype proof.
//! - Non-index keys and out-of-range accesses use ordinary property semantics.
//! - Once exposed to a getter, every later access uses the same live object.
//! - Keys are rooted before materialization can trigger moving collection.
//!
//! # See also
//! - `jit_spread_call_ops` owns canonical arguments materialization.
//! - `otter-compiler/src/arguments_elision.rs` proves the identity cannot escape.

use crate::rooting::RootScopeExt;
use crate::{
    ActivationStack, ActiveFrameMut, ActiveFrameRef, ExecutionContext, Interpreter, Value, VmError,
};

impl Interpreter {
    fn unmaterialized_argument_read(
        &self,
        stack: &ActivationStack,
        frame: &ActiveFrameRef<'_>,
        materialized: Option<usize>,
        key: Option<Value>,
    ) -> Result<Option<Value>, VmError> {
        let Some(count) = self.elided_forward_argument_count(stack, frame, materialized) else {
            return Ok(None);
        };
        let Some(key) = key else {
            return Ok(Some(Value::number(crate::NumberValue::from_f64(
                f64::from(count),
            ))));
        };
        let Some(index) = key.as_i32().and_then(|index| usize::try_from(index).ok()) else {
            return Ok(None);
        };
        if index >= count as usize {
            return Ok(None);
        }
        let value = if frame.incoming_argument_count().is_some() {
            frame.incoming_argument(index)?
        } else {
            *materialized
                .and_then(|index| stack.get(index))
                .and_then(|frame| self.frame_cold(frame))
                .and_then(|cold| cold.incoming_args.get(index))
                .ok_or(VmError::InvalidOperand)?
        };
        Ok(Some(value))
    }

    fn materialized_argument_read(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        object: Value,
        key: Option<Value>,
    ) -> Result<Value, VmError> {
        match key {
            None => self.get_property_value_for_call(stack, context, object, "length"),
            Some(key) => self.load_element_values(stack, context, object, key),
        }
    }

    pub(crate) fn read_frame_arguments(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        index: usize,
        key: Option<Value>,
    ) -> Result<Value, VmError> {
        if let Some(value) = self.unmaterialized_argument_read(
            stack,
            &ActiveFrameRef::materialized(&stack[index]),
            Some(index),
            key,
        )? {
            return Ok(value);
        }
        let mut rooted_key = key.unwrap_or_else(Value::undefined);
        let mut roots = otter_gc::RootScope::new(&mut self.gc_heap);
        // SAFETY: the stack slot remains stationary until roots is dropped.
        unsafe {
            roots.add_value(&mut rooted_key);
        }
        let object = self.materialize_frame_arguments_object(context, stack, index)?;
        self.materialized_argument_read(context, stack, object, key.map(|_| rooted_key))
    }

    pub(crate) fn jit_read_arguments(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        frame: &mut ActiveFrameMut<'_>,
        materialized: Option<usize>,
        key: Option<Value>,
    ) -> Result<Value, VmError> {
        if let Some(value) =
            self.unmaterialized_argument_read(stack, &frame.as_ref(), materialized, key)?
        {
            return Ok(value);
        }
        let mut rooted_key = key.unwrap_or_else(Value::undefined);
        let mut roots = otter_gc::RootScope::new(&mut self.gc_heap);
        // SAFETY: the stack slot remains stationary until roots is dropped.
        unsafe {
            roots.add_value(&mut rooted_key);
        }
        let object = self.jit_materialize_arguments(context, stack, frame, materialized)?;
        self.materialized_argument_read(context, stack, object, key.map(|_| rooted_key))
    }
}
