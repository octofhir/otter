//! Native lazy globals: declaration-derived deferred installation.
//!
//! An extension's JS half (class shims, pure-JS members) is parsed
//! and evaluated only when one of the globals it defines is first
//! touched. Each registered group installs one native accessor per
//! name on `globalThis`; the first read of any of them evaluates the
//! group's source exactly once (through indirect `eval`, so the
//! ordinary compile pipeline and global-scope semantics apply) and
//! the sources' own installers replace the accessors with real data
//! properties. Assignment before materialization shadows the
//! accessor with a plain data property, matching platform semantics.
//!
//! This replaces the string-built `(0, eval)` accessor shim: the name
//! list arrives from the extension declaration, the accessors are
//! native functions, and the group state lives on the interpreter —
//! no hand-maintained registry, no source-string choreography.
//!
//! # Contents
//! - [`Interpreter::register_lazy_global_group`] — install the
//!   accessors for one group.
//! - [`LazyGlobalGroup`] — per-isolate group state.
//!
//! # Invariants
//! - Materialization is once-per-group and re-entrancy-safe: the done
//!   flag flips and every accessor of the group is deleted *before*
//!   the source runs, so a global read during evaluation observes
//!   `undefined` instead of looping through the getter.
//! - Group state is per-interpreter — no process globals.
//!
//! # See also
//! - `EXTENSION_API_PLAN.md` §5/§6.7 — the design.
//! - `crates/otter-runtime` `Extension` — the declaration that feeds
//!   this registry.

use crate::{Interpreter, NativeCtx, NativeError, Value, VmError, object};

/// One registered group: the names its source defines, the source
/// itself (taken on materialization), and the once flag.
#[derive(Debug)]
pub(crate) struct LazyGlobalGroup {
    names: Vec<&'static str>,
    source: Option<String>,
    done: bool,
}

impl Interpreter {
    /// Register a lazy global group: one native accessor per name on
    /// `globalThis`, all materializing `source` on first touch.
    pub fn register_lazy_global_group(
        &mut self,
        names: Vec<&'static str>,
        source: String,
    ) -> Result<(), VmError> {
        let index = self.lazy_global_groups.len();
        self.lazy_global_groups.push(LazyGlobalGroup {
            names: names.clone(),
            source: Some(source),
            done: false,
        });
        let global = *self.global_this();
        self.with_handle_scope(|interp, scope| {
            let global_handle = interp.scoped_value(scope, Value::object(global));
            for name in &names {
                let group_index = interp.scoped_number(scope, index as f64);
                let name_value = interp.scoped_string(scope, name)?;
                let getter = interp.scoped_native_captured(
                    scope,
                    "lazyGlobalGet",
                    0,
                    &[group_index, name_value],
                    lazy_global_getter,
                )?;
                let setter = interp.scoped_native_captured(
                    scope,
                    "lazyGlobalSet",
                    1,
                    &[name_value],
                    lazy_global_setter,
                )?;
                interp.scoped_define_accessor(
                    scope,
                    global_handle,
                    name,
                    Some(getter),
                    Some(setter),
                    crate::Attr::new(false, false, true).to_flags(),
                )?;
            }
            Ok(())
        })
    }

    pub(crate) fn lazy_global_group_pending(&self, index: usize) -> bool {
        self.lazy_global_groups
            .get(index)
            .is_some_and(|group| !group.done)
    }

    /// Flip the once flag and take the group's names + source for
    /// materialization.
    pub(crate) fn lazy_global_group_begin(
        &mut self,
        index: usize,
    ) -> Option<(Vec<&'static str>, String)> {
        let group = self.lazy_global_groups.get_mut(index)?;
        if group.done {
            return None;
        }
        group.done = true;
        let source = group.source.take()?;
        Some((group.names.clone(), source))
    }
}

/// Getter half of a lazy global accessor. Captures: `[group_index,
/// name]`. Materializes the group once, then reads the (now real)
/// global back.
fn lazy_global_getter(
    ctx: &mut NativeCtx<'_>,
    _args: &[Value],
    captures: &[Value],
) -> Result<Value, NativeError> {
    const OP: &str = "lazy global getter";
    let index = captures
        .first()
        .and_then(|value| value.as_f64())
        .ok_or(NativeError::TypeError {
            name: OP,
            reason: "missing group capture".to_string(),
        })? as usize;
    let name = capture_string(ctx, captures, 1, OP)?;

    if ctx.interp_mut().lazy_global_group_pending(index) {
        materialize_group(ctx, index, OP)?;
    }
    let global = *ctx.interp_mut().global_this();
    Ok(object::get(global, ctx.heap(), &name).unwrap_or_else(Value::undefined))
}

/// Setter half: assignment before materialization shadows the
/// accessor with an ordinary data property (platform `[Replaceable]`
/// shape). Captures: `[name]`.
fn lazy_global_setter(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    captures: &[Value],
) -> Result<Value, NativeError> {
    const OP: &str = "lazy global setter";
    let name = capture_string(ctx, captures, 0, OP)?;
    let assigned = args.first().copied().unwrap_or_else(Value::undefined);
    let global = *ctx.interp_mut().global_this();
    ctx.scope(|ctx, s| {
        let interp = ctx.interp_mut();
        let global_handle = interp.scoped_value(s, Value::object(global));
        let value_handle = interp.scoped_value(s, assigned);
        interp
            .scoped_define_data(
                s,
                global_handle,
                &name,
                value_handle,
                crate::Attr::new(true, false, true).to_flags(),
            )
            .map_err(|err| crate::native_function::vm_to_native_error(interp, err, OP))
    })?;
    Ok(Value::undefined())
}

/// Evaluate a group's source once: delete every accessor of the group
/// (so reads during evaluation see `undefined`, never the getter),
/// then run the source through indirect `eval` in global scope.
fn materialize_group(
    ctx: &mut NativeCtx<'_>,
    index: usize,
    op: &'static str,
) -> Result<(), NativeError> {
    let Some((names, source)) = ctx.interp_mut().lazy_global_group_begin(index) else {
        return Ok(());
    };
    let global = *ctx.interp_mut().global_this();
    for name in &names {
        object::delete(global, ctx.heap_mut(), name);
    }
    let Some(eval_fn) = ctx.global_value("eval") else {
        return Err(NativeError::TypeError {
            name: op,
            reason: "global eval is unavailable; cannot materialize lazy globals".to_string(),
        });
    };
    ctx.scope(|ctx, s| {
        let mut cx = crate::marshal::MarshalCx::new(ctx, s);
        let eval_handle = cx.park(eval_fn);
        let source_handle = cx.string(&source).map_err(|err| err.into_native(op))?;
        let receiver = cx.undefined();
        cx.call(eval_handle, receiver, &[source_handle])
            .map_err(|err| err.into_native(op))?;
        Ok(())
    })
}

fn capture_string(
    ctx: &NativeCtx<'_>,
    captures: &[Value],
    index: usize,
    op: &'static str,
) -> Result<String, NativeError> {
    captures
        .get(index)
        .and_then(|value| value.as_string(ctx.heap()))
        .map(|string| string.to_lossy_string(ctx.heap()))
        .ok_or(NativeError::TypeError {
            name: op,
            reason: "missing name capture".to_string(),
        })
}
