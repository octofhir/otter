//! Iterative, handle-scoped FlattenIntoArray for `flat` and `flatMap`.
//!
//! # Contents
//! - Depth-first source frames with live HasProperty/Get and target writes.
//! - Top-level mapper invocation and full captured ToLength ranges.
//! - Native work checkpoints and catchable exhaustion of 512 active frames.
//!
//! # Invariants
//! - No source nesting consumes the Rust call stack.
//! - Frame handles are reused by depth and cleared when a source completes;
//!   root storage is bounded by nesting, never the number of source elements.
//! - Every attempted index charges work before any observable property access.
//! - Target, sources, mapper, receiver and current element remain scoped roots
//!   across all collecting or reentrant operations.
//! - Only the top frame applies the mapper; children retain depth-first order.
//!
//! # See also
//! - `super::Interpreter::array_flat` owns depth coercion and species creation.
//! - `super::array_callback_native_dispatch` owns flatMap callback validation.

use smallvec::{SmallVec, smallvec};

use crate::handles::Local;
use crate::native_abi::CommittedValueError;
use crate::{ActivationStack, ExecutionContext, Interpreter, Value};

use super::{MAX_SAFE_ARRAY_LENGTH, format_index_key, length_of_array_like};

const MAX_FLATTEN_FRAMES: usize = 512;

#[derive(Clone, Copy)]
struct Frame<'s> {
    source: Local<'s>,
    len: usize,
    next: usize,
    depth: i64,
}

impl Interpreter {
    /// Complete FlattenIntoArray with bounded native depth and rooted cursors.
    pub(crate) fn flatten_into_array(
        &mut self,
        stack: &mut ActivationStack,
        context: &ExecutionContext,
        target: Value,
        source: Value,
        source_len: usize,
        start: usize,
        depth: i64,
        mapper: Option<(Value, Value)>,
    ) -> Result<usize, CommittedValueError> {
        self.with_handle_scope(|interp, scope| {
            let target = interp.scoped_value(scope, target);
            let source = interp.scoped_value(scope, source);
            let element = interp.scoped_value(scope, Value::undefined());
            let mapper = mapper.map(|(function, this_arg)| {
                (
                    interp.scoped_value(scope, function),
                    interp.scoped_value(scope, this_arg),
                )
            });
            let mut frames: SmallVec<[Frame<'_>; 4]> = smallvec![Frame {
                source,
                len: source_len,
                next: 0,
                depth,
            }];
            let mut level = 0;
            let mut target_index = start;
            loop {
                let frame = frames[level];
                if frame.next == frame.len {
                    interp.set_scoped(frame.source, Value::undefined());
                    if level == 0 {
                        return Ok(target_index);
                    }
                    level -= 1;
                    continue;
                }
                interp
                    .charge_native_loop_iteration()
                    .map_err(CommittedValueError::Fatal)?;
                let index = frame.next;
                frames[level].next += 1;
                let key = format_index_key(index as f64);
                let source = interp.escape_scoped(frame.source);
                if !interp.array_method_has_property(stack, context, source, &key)? {
                    continue;
                }
                let source = interp.escape_scoped(frame.source);
                let value = interp.array_method_get_property(stack, context, source, &key)?;
                interp.set_scoped(element, value);
                if level == 0
                    && let Some((function, this_arg)) = mapper
                {
                    let function = interp.escape_scoped(function);
                    let this_arg = interp.escape_scoped(this_arg);
                    let value = interp.escape_scoped(element);
                    let source = interp.escape_scoped(frame.source);
                    let value = interp
                        .run_callable_sync_rooted(
                            stack,
                            Some(context),
                            &function,
                            this_arg,
                            smallvec![value, Value::number_f64(index as f64), source],
                        )
                        .map_err(CommittedValueError::completed_call)?;
                    interp.set_scoped(element, value);
                }
                let value = interp.escape_scoped(element);
                if frame.depth > 0
                    && interp
                        .is_array(&value)
                        .map_err(CommittedValueError::JavaScript)?
                {
                    let value = interp.escape_scoped(element);
                    let len = length_of_array_like(interp, stack, context, &value)?;
                    if len != 0 {
                        if level + 1 == MAX_FLATTEN_FRAMES {
                            return Err(CommittedValueError::JavaScript(
                                interp.err_range("Maximum array flattening depth exceeded".into()),
                            ));
                        }
                        level += 1;
                        let value = interp.escape_scoped(element);
                        let source = if level == frames.len() {
                            let source = interp.scoped_value(scope, value);
                            frames.push(Frame {
                                source,
                                len,
                                next: 0,
                                depth: frame.depth - 1,
                            });
                            source
                        } else {
                            let source = frames[level].source;
                            frames[level] = Frame {
                                source,
                                len,
                                next: 0,
                                depth: frame.depth - 1,
                            };
                            source
                        };
                        interp.set_scoped(source, value);
                    }
                } else {
                    if target_index >= MAX_SAFE_ARRAY_LENGTH {
                        return Err(CommittedValueError::JavaScript(interp.err_type(
                            "flatten target index exceeds maximum safe integer".into(),
                        )));
                    }
                    let target = interp.escape_scoped(target);
                    let value = interp.escape_scoped(element);
                    interp.create_data_property_or_throw(
                        stack,
                        context,
                        target,
                        &format_index_key(target_index as f64),
                        value,
                    )?;
                    target_index += 1;
                }
                interp.set_scoped(element, Value::undefined());
            }
        })
    }
}
