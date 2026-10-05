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

use crate::native_abi::CommittedValueError;
use crate::rooting::RootScopeExt;
use crate::{
    ActivationStack, ActiveFrameMut, ActiveFrameRef, ExecutionContext, Interpreter, Value, VmError,
};

impl Interpreter {
    fn unmaterialized_argument_read(
        &self,
        frame: &ActiveFrameRef<'_>,
        key: Option<Value>,
    ) -> Result<Option<Value>, VmError> {
        let Some(count) = self.elided_forward_argument_count(frame) else {
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
        let value = frame.incoming_argument(index)?;
        Ok(Some(value))
    }

    fn materialized_argument_read(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        object: Value,
        key: Option<Value>,
    ) -> Result<Value, CommittedValueError> {
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
    ) -> Result<Value, CommittedValueError> {
        if let Some(value) = self
            .unmaterialized_argument_read(&ActiveFrameRef::from_frame(&stack[index]), key)
            .map_err(|error| CommittedValueError::JavaScript(error.into()))?
        {
            return Ok(value);
        }
        let mut rooted_key = key.unwrap_or_else(Value::undefined);
        let mut roots = otter_gc::RootScope::new(&mut self.gc_heap);
        // SAFETY: the stack slot remains stationary until roots is dropped.
        unsafe {
            roots.add_value(&mut rooted_key);
        }
        let object = self
            .materialize_frame_arguments_object(context, stack, index)
            .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
        self.materialized_argument_read(context, stack, object, key.map(|_| rooted_key))
    }

    pub(crate) fn jit_read_arguments(
        &mut self,
        context: &ExecutionContext,
        stack: &mut ActivationStack,
        frame: &mut ActiveFrameMut<'_>,
        key: Option<Value>,
    ) -> Result<Value, CommittedValueError> {
        if let Some(value) = self
            .unmaterialized_argument_read(&frame.as_ref(), key)
            .map_err(|error| CommittedValueError::JavaScript(error.into()))?
        {
            return Ok(value);
        }
        let mut rooted_key = key.unwrap_or_else(Value::undefined);
        let mut roots = otter_gc::RootScope::new(&mut self.gc_heap);
        // SAFETY: the stack slot remains stationary until roots is dropped.
        unsafe {
            roots.add_value(&mut rooted_key);
        }
        let object = self
            .jit_materialize_arguments(context, stack, frame)
            .map_err(|error| CommittedValueError::JavaScript(error.into()))?;
        self.materialized_argument_read(context, stack, object, key.map(|_| rooted_key))
    }
}
