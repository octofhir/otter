//! Array static opcode helpers.
//!
//! Array constructor/static bytecodes are variadic, so their argument registers
//! still live in the executable side-operand slice. This module keeps their
//! decode and call glue out of the main interpreter loop.
//!
//! # Contents
//! - `Array(...)` / `new Array(...)` construction.
//! - `Array.from(...)` and `Array.of(...)` static calls.
//!
//! # Invariants
//! - The current frame PC is advanced before running `Array.from` so any
//!   synchronous iterator/property callbacks observe the post-call PC.
//! - Arguments are read from executable operands, not cloned bytecode DTOs.
//!
//! # See also
//! - [`crate::array_statics`]
//! - [`crate::executable`]

use otter_bytecode::{Op, Operand};
use smallvec::SmallVec;

use crate::{
    ExecutionContext, Frame, Interpreter, Value, VmError, VmGetOutcome, VmPropertyKey, array,
    array_statics, number, operand_decode::register_operand, read_register, symbol, to_length,
    write_register,
};

impl Interpreter {
    pub(crate) fn run_array_static_operands(
        &mut self,
        op: Op,
        context: &ExecutionContext,
        stack: &mut SmallVec<[Frame; 8]>,
        operands: &[Operand],
    ) -> Result<(), VmError> {
        let dst = register_operand(operands.first())?;
        let top_idx = stack.len() - 1;
        let args = collect_array_args(&stack[top_idx], operands)?;

        let pc = stack[top_idx].pc;
        stack[top_idx].pc = pc.checked_add(1).ok_or(VmError::InvalidOperand)?;
        let result = match op {
            Op::ArrayConstruct => array_statics::construct(&args, &mut self.gc_heap)?,
            Op::ArrayFrom => self.array_from_sync(context, &args)?,
            Op::ArrayOf => array_statics::of(&args, &mut self.gc_heap)?,
            _ => return Err(VmError::InvalidOperand),
        };

        let frame = stack.last_mut().ok_or(VmError::InvalidOperand)?;
        write_register(frame, dst, result)
    }

    /// §23.1.2.1 `Array.from(items, mapFn?, thisArg?)`.
    ///
    /// Splits on `items`:
    /// - Has `@@iterator` → walk via [`Self::iterator_to_list_sync`]
    ///   (sync iterator protocol, §7.4).
    /// - Otherwise → array-like read of `length` + indexed
    ///   properties (§7.3.18 CreateListFromArrayLike with no element
    ///   type filter).
    ///
    /// When `mapFn` is supplied (must be callable), each value is
    /// passed through `mapFn(value, index)` with `this` = `thisArg`.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-array.from>
    pub(crate) fn array_from_sync(
        &mut self,
        context: &ExecutionContext,
        args: &[Value],
    ) -> Result<Value, VmError> {
        let items = args.first().cloned().unwrap_or(Value::Undefined);
        let map_fn = args.get(1).cloned().unwrap_or(Value::Undefined);
        let this_arg = args.get(2).cloned().unwrap_or(Value::Undefined);
        let has_map = !matches!(map_fn, Value::Undefined);
        if has_map && !self.is_callable_runtime(&map_fn) {
            return Err(VmError::TypeError {
                message: "Array.from mapFn must be callable".to_string(),
            });
        }

        // Step 1 — built-in iterable fast paths short-circuit the
        // `@@iterator` round-trip; for everything else look up
        // `@@iterator` to decide between iterable and array-like.
        let is_builtin_iterable = matches!(
            items,
            Value::Array(_)
                | Value::String(_)
                | Value::Set(_)
                | Value::Map(_)
                | Value::Generator(_)
        );
        let iterator_method = if matches!(items, Value::Undefined | Value::Null) {
            Value::Undefined
        } else if is_builtin_iterable {
            // Sentinel: any non-undefined value picks the iterator
            // path below; `iterator_to_list_sync` handles built-ins
            // via its fast-path branches.
            Value::Boolean(true)
        } else {
            let iterator_sym = self.well_known_symbols.get(symbol::WellKnown::Iterator);
            match self.ordinary_get_value(
                context,
                items.clone(),
                items.clone(),
                &VmPropertyKey::Symbol(iterator_sym),
                0,
            )? {
                VmGetOutcome::Value(v) => v,
                VmGetOutcome::InvokeGetter { getter } => {
                    self.run_callable_sync(context, &getter, items.clone(), SmallVec::new())?
                }
            }
        };

        let raw_values: Vec<Value> = if matches!(iterator_method, Value::Undefined | Value::Null) {
            // Step 4 — ArrayLike path.
            if matches!(items, Value::Undefined | Value::Null) {
                return Err(VmError::TypeError {
                    message: "Array.from requires an iterable or array-like".to_string(),
                });
            }
            let length_value = match self.ordinary_get_value(
                context,
                items.clone(),
                items.clone(),
                &VmPropertyKey::String("length".to_string()),
                0,
            )? {
                VmGetOutcome::Value(v) => v,
                VmGetOutcome::InvokeGetter { getter } => {
                    self.run_callable_sync(context, &getter, items.clone(), SmallVec::new())?
                }
            };
            let len = to_length(&length_value)?;
            let mut out = Vec::with_capacity(len);
            for index in 0..len {
                let key = VmPropertyKey::String(index.to_string());
                let value = match self.ordinary_get_value(
                    context,
                    items.clone(),
                    items.clone(),
                    &key,
                    0,
                )? {
                    VmGetOutcome::Value(v) => v,
                    VmGetOutcome::InvokeGetter { getter } => {
                        self.run_callable_sync(context, &getter, items.clone(), SmallVec::new())?
                    }
                };
                out.push(value);
            }
            out
        } else {
            if !is_builtin_iterable && !self.is_callable_runtime(&iterator_method) {
                return Err(VmError::TypeError {
                    message: "iterator method is not callable".to_string(),
                });
            }
            // `iterator_to_list_sync` short-circuits built-ins and
            // routes everything else through `GetIterator` /
            // `IteratorStep`.
            self.iterator_to_list_sync(context, &items)?
        };

        let mut mapped: Vec<Value> = Vec::with_capacity(raw_values.len());
        for (index, value) in raw_values.into_iter().enumerate() {
            if has_map {
                let mut cb_args: SmallVec<[Value; 8]> = SmallVec::new();
                cb_args.push(value);
                cb_args.push(Value::Number(number::NumberValue::from_i32(index as i32)));
                let mapped_value =
                    self.run_callable_sync(context, &map_fn, this_arg.clone(), cb_args)?;
                mapped.push(mapped_value);
            } else {
                mapped.push(value);
            }
        }
        Ok(Value::Array(array::from_elements(
            &mut self.gc_heap,
            mapped,
        )?))
    }
}

fn collect_array_args(
    frame: &Frame,
    operands: &[Operand],
) -> Result<SmallVec<[Value; 4]>, VmError> {
    let argc = match operands.get(1) {
        Some(&Operand::ConstIndex(n)) => n as usize,
        _ => return Err(VmError::InvalidOperand),
    };
    let mut args: SmallVec<[Value; 4]> = SmallVec::with_capacity(argc);
    for i in 0..argc {
        let r = register_operand(operands.get(2 + i))?;
        args.push(read_register(frame, r)?.clone());
    }
    Ok(args)
}
