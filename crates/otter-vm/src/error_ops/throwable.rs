//! Canonical fallible projection from VM failures to JavaScript exceptions.
//!
//! # Contents
//! - Intrinsic Error class allocation and descriptor publication.
//! - Existing rooted user-throw consumption and source-realm selection.
//!
//! # Invariants
//! - Structural/control errors never become JavaScript values.
//! - Every allocating step returns its exact typed failure; no string or
//!   undefined fallback can hide heap exhaustion.
//! - Immediate synchronous user exceptions retain their identity through the
//!   VM's one pending-throw root. Deferred owned text carries no JS identity.
//! - Created errors use pinned realm intrinsics and scoped allocation.
//! - Successful projection consumes the handled error detail and stack provenance
//!   before a later reaction can raise another failure. Fatal early returns retain
//!   their detail; materialization consumes the original detail before allocation
//!   so a new allocation failure cannot inherit it. Source frames survive failure.
//!
//! # See also
//! - [`super::native_to_vm_error_with_stack`]
//! - [`crate::marshal::JsError`]

use super::{coded_error_to_string, system_error_code};
use crate::{ActivationStack, ExecutionContext, Interpreter, Value, VmError};
use crate::{error_classes, object};

impl Interpreter {
    /// Project one VM failure to its exact JavaScript exception value.
    /// Structural/control failures return unchanged; materialization failure
    /// returns its actual typed cause. A pending user throw is consumed only
    /// for Uncaught, preserving object/symbol identity without reconstruction.
    pub(crate) fn vm_error_to_throwable_with_stack_roots(
        &mut self,
        context: Option<&ExecutionContext>,
        stack: &ActivationStack,
        err: &VmError,
    ) -> Result<Value, VmError> {
        let value = self.materialize_vm_throwable(context, stack, err)?;
        // This error has become a handled JavaScript value. Its provenance is
        // no longer in flight; a subsequent reaction may fail without setting
        // dynamic detail (for example InvalidOperand).
        let _ = self.take_error_detail();
        let _ = self.pending_uncaught_frames.take();
        Ok(value)
    }

    fn materialize_vm_throwable(
        &mut self,
        context: Option<&ExecutionContext>,
        stack: &ActivationStack,
        err: &VmError,
    ) -> Result<Value, VmError> {
        if err.is_fatal() {
            return Err(*err);
        }
        if matches!(err, VmError::Uncaught) {
            if let Some(value) = self.take_pending_uncaught_throw() {
                let _ = self.take_error_detail();
                return Ok(value);
            }
            // A text-only host throw has no JavaScript cell to preserve. Its
            // owned payload becomes the string value; diagnostic prefixes are
            // not part of that value. Consume it before the allocator so a new
            // failure cannot inherit this handled throw's detail.
            let message = match self.take_error_detail() {
                Some(crate::ErrorDetail::Uncaught(message)) => message.into_string(),
                _ => err.to_string(),
            };
            return self.with_handle_scope(|interp, scope| {
                let string = interp.scoped_string(scope, &message)?;
                Ok(interp.escape_scoped(string))
            });
        }
        // ActivationStack indexes JavaScript frames and skips physical Host
        // records. Inspect only the actual innermost published frame; a Host
        // error belongs to the creation realm that is still active here.
        // SAFETY: the current runtime extent retains its initialized published
        // chain; this immutable domain read cannot collect or reenter.
        let physical_host = unsafe { self.jit_innermost_native_frame().as_ref() }
            .is_some_and(|frame| frame.header.kind == crate::native_abi::NativeFrameKind::Host);
        let error_realm_id = if physical_host {
            self.active_realm_id
        } else {
            stack
                .last()
                .and_then(|frame| self.function_realm_ids.get(&frame.function_id))
                .copied()
                .unwrap_or(self.active_realm_id)
        };
        if error_realm_id != self.active_realm_id {
            return self.with_host_realm_id(error_realm_id, |interp| {
                interp.vm_error_to_throwable_with_stack_roots(context, stack, err)
            });
        }
        use crate::run_control::ErrorDetail;
        let is_oom = matches!(err, VmError::OutOfMemory { .. });
        // Node-style `.code` to stamp on the instance after it is built.
        let mut node_code: Option<&'static str> = None;
        // `VmError` is `Copy`; its dynamic message/payload lives in the isolate
        // pending-error slot. Consume it once, paired with the discriminant,
        // before any allocator can return a different failure.
        let detail = self.take_error_detail();
        let msg_detail = || match &detail {
            Some(ErrorDetail::Message(m)) => m.to_string(),
            Some(ErrorDetail::Name(m)) => m.to_string(),
            Some(ErrorDetail::Uncaught(m)) => m.to_string(),
            _ => String::new(),
        };
        // System-call properties (`errno`, `syscall`, `path`, `dest`) to stamp
        // alongside the code.
        let mut syscall_detail: Option<crate::run_control::VmSyscallError> = None;
        let dynamic_message: String;
        let (kind, message): (error_classes::ErrorKind, &str) = match err {
            VmError::Coded => {
                if let Some(ErrorDetail::Syscall(payload)) = &detail {
                    node_code = Some(payload.code);
                    dynamic_message = payload.message.clone();
                    syscall_detail = Some(payload.clone());
                    (error_classes::ErrorKind::Error, dynamic_message.as_str())
                } else if let Some(ErrorDetail::Coded(payload)) = &detail {
                    node_code = Some(payload.code);
                    dynamic_message = payload.message.clone();
                    (payload.kind, dynamic_message.as_str())
                } else {
                    dynamic_message = msg_detail();
                    (error_classes::ErrorKind::Error, dynamic_message.as_str())
                }
            }
            VmError::TypeMismatch => (
                error_classes::ErrorKind::TypeError,
                "type mismatch: this operation does not accept a value of this type",
            ),
            VmError::TypeMismatchAt => {
                dynamic_message = match &detail {
                    Some(ErrorDetail::Mismatch(p)) => {
                        format!("{}: cannot operate on a value of type {}", p.op, p.kind)
                    }
                    _ => "TypeError".to_string(),
                };
                (
                    error_classes::ErrorKind::TypeError,
                    dynamic_message.as_str(),
                )
            }
            VmError::TypeError => {
                dynamic_message = msg_detail();
                (
                    error_classes::ErrorKind::TypeError,
                    dynamic_message.as_str(),
                )
            }
            VmError::RangeError => {
                dynamic_message = msg_detail();
                (
                    error_classes::ErrorKind::RangeError,
                    dynamic_message.as_str(),
                )
            }
            VmError::SyntaxError => {
                dynamic_message = msg_detail();
                (
                    error_classes::ErrorKind::SyntaxError,
                    dynamic_message.as_str(),
                )
            }
            VmError::URIError => {
                dynamic_message = msg_detail();
                (error_classes::ErrorKind::URIError, dynamic_message.as_str())
            }
            VmError::NotCallable => (
                error_classes::ErrorKind::TypeError,
                "value is not a function",
            ),
            VmError::TemporalDeadZone { .. } => (
                error_classes::ErrorKind::ReferenceError,
                "cannot access binding before initialization",
            ),
            VmError::ThisUninitialized => {
                dynamic_message = msg_detail();
                (
                    error_classes::ErrorKind::ReferenceError,
                    dynamic_message.as_str(),
                )
            }
            VmError::UndefinedIdentifier => {
                dynamic_message = match &detail {
                    Some(ErrorDetail::Name(name)) => format!("{name} is not defined"),
                    _ => "identifier is not defined".to_string(),
                };
                (
                    error_classes::ErrorKind::ReferenceError,
                    dynamic_message.as_str(),
                )
            }
            VmError::UnknownIntrinsic => (
                error_classes::ErrorKind::TypeError,
                "unknown intrinsic method",
            ),
            VmError::OutOfMemory { .. } => {
                dynamic_message = err.to_string();
                (
                    error_classes::ErrorKind::RangeError,
                    dynamic_message.as_str(),
                )
            }
            // §25.5 JSON.parse / JSON.stringify spec-mandated
            // exception classes:
            //   parse failures → SyntaxError (§25.5.1.1 step 2),
            //   cyclic / BigInt / depth / bad-arg → TypeError.
            VmError::JsonError => {
                let (jkind, jmsg) = match &detail {
                    Some(ErrorDetail::Json(payload)) => {
                        let kind = if payload.code == "JSON_PARSE" {
                            error_classes::ErrorKind::SyntaxError
                        } else {
                            error_classes::ErrorKind::TypeError
                        };
                        (kind, payload.message.clone())
                    }
                    _ => (error_classes::ErrorKind::TypeError, String::new()),
                };
                dynamic_message = jmsg;
                (jkind, dynamic_message.as_str())
            }
            // A blown call stack is a `RangeError` user code can catch, the
            // way V8 answers one — Node's own stdlib and its tests recurse
            // until it throws and then carry on. The instance is built by
            // native allocation alone, so it needs none of the frames the
            // exhausted stack can no longer hand out.
            VmError::StackOverflow { .. } => (
                error_classes::ErrorKind::RangeError,
                "Maximum call stack size exceeded",
            ),
            VmError::InvalidRegExp => {
                dynamic_message = msg_detail();
                (
                    error_classes::ErrorKind::SyntaxError,
                    dynamic_message.as_str(),
                )
            }
            VmError::Uncaught
            | VmError::ResourceLimit
            | VmError::MissingReturn
            | VmError::InvalidOperand
            | VmError::Interrupted
            | VmError::BudgetExceeded
            | VmError::Exit { .. } => {
                return Err(*err);
            }
        };
        let mut obj = if is_oom {
            let proto = self.error_classes.prototype(kind);
            // The bootstrap finalizer publishes this exact ordinary/default
            // capacity variant before an exhausted-heap diagnostic can run.
            // Reading the cache performs no allocating state preparation.
            let head = crate::object::cached_instance_root(proto, &self.gc_heap)
                .ok_or(VmError::InvalidOperand)?;
            let root = crate::object::shape_body::root_for_layout(
                head,
                crate::object::DEFAULT_INLINE_CAPACITY,
                crate::object::ShapeState::ORDINARY,
            )
            .ok_or(VmError::InvalidOperand)?;
            crate::object::alloc_diagnostic_object(&mut self.gc_heap, root)?
        } else {
            self.make_error_instance_with_stack_roots(
                stack,
                kind,
                Some(message.to_string()),
                &Value::undefined(),
            )?
        };
        // Descriptor publication shares one handle scope. Every fallible
        // string, callable, child object and descriptor operation propagates
        // its actual allocation cause before the exception is returned.
        obj = self.with_handle_scope(|interp, scope| {
            let obj_h = interp.scoped_value(scope, Value::object(obj));
            if is_oom {
                let message_h = interp.scoped_string(scope, message)?;
                interp.scoped_define_data(
                    scope,
                    obj_h,
                    "message",
                    message_h,
                    object::PropertyFlags::new(true, false, true),
                )?;
            }
            if let Some(code) = node_code {
                let code_h = interp.scoped_string(scope, code)?;
                interp.scoped_define_data(
                    scope,
                    obj_h,
                    "code",
                    code_h,
                    object::PropertyFlags::new(true, false, true),
                )?;
                if code.starts_with("ERR_") {
                    let to_string = crate::native_function::native_value_static(
                        &mut interp.gc_heap,
                        "toString",
                        0,
                        coded_error_to_string,
                    )?;
                    let to_string_h = interp.scoped_value(scope, to_string);
                    interp.scoped_define_data(
                        scope,
                        obj_h,
                        "toString",
                        to_string_h,
                        object::PropertyFlags::new(true, false, true),
                    )?;
                }
                if let Some(payload) = &syscall_detail {
                    let errno = interp.scoped_value(
                        scope,
                        Value::number(crate::NumberValue::from_f64(f64::from(payload.errno))),
                    );
                    interp.scoped_define_data(
                        scope,
                        obj_h,
                        "errno",
                        errno,
                        object::PropertyFlags::new(true, false, true),
                    )?;
                    let syscall_h = interp.scoped_string(scope, payload.syscall)?;
                    interp.scoped_define_data(
                        scope,
                        obj_h,
                        "syscall",
                        syscall_h,
                        object::PropertyFlags::new(true, false, true),
                    )?;
                    for (name, value) in [("path", &payload.path), ("dest", &payload.dest)] {
                        let Some(value) = value else { continue };
                        let value_h = interp.scoped_string(scope, value)?;
                        interp.scoped_define_data(
                            scope,
                            obj_h,
                            name,
                            value_h,
                            object::PropertyFlags::new(true, false, true),
                        )?;
                    }
                }
                if code == "ERR_SYSTEM_ERROR" {
                    let name_h = interp.scoped_string(scope, "SystemError")?;
                    interp.scoped_define_data(
                        scope,
                        obj_h,
                        "name",
                        name_h,
                        object::PropertyFlags::new(true, false, true),
                    )?;
                    let info_h = interp.scoped_object_bare(scope)?;
                    let info_code_h = interp.scoped_string(scope, system_error_code(message))?;
                    interp.scoped_define_data(
                        scope,
                        info_h,
                        "code",
                        info_code_h,
                        object::PropertyFlags::data_default(),
                    )?;
                    interp.scoped_define_data(
                        scope,
                        obj_h,
                        "info",
                        info_h,
                        object::PropertyFlags::new(true, false, true),
                    )?;
                }
            }
            interp
                .escape_scoped(obj_h)
                .as_object()
                .ok_or(VmError::InvalidOperand)
        })?;
        // Engine-raised errors carry the same construction-site call stack
        // as user `new Error(...)` instances, so `e.stack` shows frames for
        // a VM TypeError exactly like V8/JSC.
        if let Some(context) = context {
            self.capture_error_stack_frames(context, &mut obj)?;
        }
        Ok(Value::object(obj))
    }
}
