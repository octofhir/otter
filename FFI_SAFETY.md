# FFI Safety Guidelines

This document defines the safety rules and review checklist for working with JavaScriptCore FFI bindings in the Otter runtime.

## Crate Boundary

### jsc-sys (Unsafe Layer)

All unsafe FFI code MUST be isolated in the `jsc-sys` crate:

- Raw type definitions: `JSContextRef`, `JSValueRef`, `JSObjectRef`, etc.
- Extern function declarations
- Platform-specific linking
- Constants and enums

**No safe wrappers in jsc-sys.** This crate is the audit boundary.

### jsc-core (Safe Layer)

Safe wrappers that use `jsc-sys` internally:

- All public APIs are safe Rust
- Unsafe blocks are minimal and documented
- RAII patterns for resource management
- Thread safety enforced by type system

## Rules for Unsafe Blocks

### 1. Isolation

All `unsafe` FFI calls MUST be in `jsc-sys` crate only. The `jsc-core` crate may use `unsafe` to call into `jsc-sys`, but:

```rust
// GOOD: Unsafe block with safety comment
unsafe {
    // SAFETY: ctx is valid, script is null-terminated, exception is initialized
    jsc_sys::JSEvaluateScript(ctx, script, null_mut(), source, 1, &mut exception)
}

// BAD: Unsafe spread across multiple functions
unsafe fn eval_unsafe() { ... }  // Don't do this
```

### 2. Documentation

Every unsafe block MUST have a `// SAFETY:` comment explaining:

- What invariants must hold
- Why those invariants are satisfied
- What could go wrong if they aren't

```rust
// SAFETY:
// - `ctx` is valid because it was created by JSGlobalContextCreate and retained
// - `value` is valid because it came from a JSC API call in this function
// - The context owns this value, so protect/unprotect is correct
unsafe {
    jsc_sys::JSValueProtect(ctx, value);
}
```

### 3. Minimization

Unsafe blocks should be as small as possible:

```rust
// GOOD: Minimal unsafe block
let result = unsafe { jsc_sys::JSValueMakeNumber(ctx, n) };
if result.is_null() {
    return Err(JscError::Internal("Failed to create number".into()));
}

// BAD: Large unsafe block
unsafe {
    let result = jsc_sys::JSValueMakeNumber(ctx, n);
    if result.is_null() {
        return Err(JscError::Internal("Failed to create number".into()));
    }
    // ... more code that doesn't need unsafe
}
```

## Ownership & Lifetimes

### JSValueRef

Values must be protected before storing across API boundaries:

```rust
impl JscValue {
    pub unsafe fn new(ctx: JSContextRef, value: JSValueRef) -> Self {
        // SAFETY: Protect value immediately to prevent GC
        if !value.is_null() {
            jsc_sys::JSValueProtect(ctx, value);
        }
        Self { value, ctx }
    }
}

impl Drop for JscValue {
    fn drop(&mut self) {
        // SAFETY: Value was protected in new(), must unprotect
        if !self.value.is_null() {
            unsafe {
                jsc_sys::JSValueUnprotect(self.ctx, self.value);
            }
        }
    }
}
```

### JSStringRef

Strings must be released after use, in the same scope:

```rust
// GOOD: Create, use, release in same scope
let js_str = unsafe { jsc_sys::JSStringCreateWithUTF8CString(cstr.as_ptr()) };
let value = unsafe { jsc_sys::JSValueMakeString(ctx, js_str) };
unsafe { jsc_sys::JSStringRelease(js_str) };

// BAD: Storing JSStringRef without release
struct BadStruct {
    str_ref: JSStringRef,  // Don't do this - will leak
}
```

### JSContextRef

Contexts must be retained/released correctly:

```rust
impl JscContext {
    pub fn new() -> JscResult<Self> {
        // SAFETY: JSGlobalContextCreate returns retained context
        let ctx = unsafe { jsc_sys::JSGlobalContextCreate(null_mut()) };
        if ctx.is_null() {
            return Err(JscError::ContextCreation("Failed to create context".into()));
        }
        Ok(Self { ctx, ... })
    }
}

impl Drop for JscContext {
    fn drop(&mut self) {
        // SAFETY: We own this context, must release
        unsafe {
            jsc_sys::JSGlobalContextRelease(self.ctx);
        }
    }
}
```

### Callbacks

Callbacks must not outlive their registered context:

```rust
// GOOD: Callback registered on context, context lifetime bounds callback
pub fn register_function(
    &self,
    name: &str,
    callback: JSObjectCallAsFunctionCallback,
) -> JscResult<()> {
    // Callback is C function pointer, no Rust lifetime to manage
    // But: caller must ensure callback doesn't access freed resources
}

// CAREFUL: Closures with captured state
// The Extension system uses Arc<dyn Fn> to ensure lifetime safety
```

## GC Interaction

### Protect Before Store

Any `JSValueRef` stored in Rust structures must be protected:

```rust
// Timer arguments stored in Vec
pub fn schedule_timer(&self, callback: JSValueRef, args: Vec<JSValueRef>) {
    // SAFETY: Protect all values before storing
    unsafe {
        jsc_sys::JSValueProtect(self.ctx, callback);
        for arg in &args {
            jsc_sys::JSValueProtect(self.ctx, *arg);
        }
    }
    self.timers.push(TimerEntry { callback, args, ... });
}
```

### Unprotect on Drop

Implement Drop to unprotect stored values:

```rust
impl Drop for TimerEntry {
    fn drop(&mut self) {
        // SAFETY: Values were protected when stored
        unsafe {
            jsc_sys::JSValueUnprotect(self.ctx, self.callback);
            for arg in &self.args {
                jsc_sys::JSValueUnprotect(self.ctx, *arg);
            }
        }
    }
}
```

### No GC During Callbacks

Don't call `JSGarbageCollect` inside callbacks:

```rust
// BAD: GC during callback can cause use-after-free
unsafe extern "C" fn bad_callback(ctx: JSContextRef, ...) -> JSValueRef {
    jsc_sys::JSGarbageCollect(ctx);  // DON'T DO THIS
    // Arguments may now be freed!
}

// GOOD: Let GC happen naturally, or schedule for later
```

### Root Promise Handlers

Promise resolve/reject functions must be protected until called:

```rust
fn create_deferred_promise(&self, ctx: JSContextRef) -> JscResult<(JSValueRef, u64)> {
    let mut resolve: JSObjectRef = null_mut();
    let mut reject: JSObjectRef = null_mut();
    let mut exception: JSValueRef = null_mut();

    let promise = unsafe {
        jsc_sys::JSObjectMakeDeferredPromise(ctx, &mut resolve, &mut reject, &mut exception)
    };

    // SAFETY: Protect resolve/reject until promise is settled
    unsafe {
        jsc_sys::JSValueProtect(ctx, resolve as JSValueRef);
        jsc_sys::JSValueProtect(ctx, reject as JSValueRef);
    }

    // Store in pending_promises map, unprotect when resolved/rejected
    ...
}
```

## Thread Safety

### Context Thread Affinity

JSC contexts are NOT thread-safe. The type system must enforce this:

```rust
pub struct JscContext {
    ctx: JSGlobalContextRef,
    _not_send: PhantomData<*mut ()>,  // Makes type !Send
}

// Explicit negative impl (requires nightly or careful design)
impl !Send for JscContext {}
impl !Sync for JscContext {}
```

### Cross-Thread Communication

Use channels for cross-thread job submission:

```rust
// GOOD: EngineHandle is Send+Sync, communicates via channel
pub struct EngineHandle {
    job_tx: crossbeam_channel::Sender<Job>,
}

// Jobs are executed on the owning thread
pub(crate) fn spawn_worker(job_rx: Receiver<Job>) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let context = JscContext::new().unwrap();  // Context created ON this thread
        loop {
            match job_rx.recv() {
                Ok(job) => execute_job(&context, job),
                Err(_) => break,
            }
        }
    })
}
```

## Exception Handling

### Check Exception After Every Call

JSC reports errors via exception out-parameter:

```rust
pub fn eval(&self, script: &str) -> JscResult<JscValue> {
    let mut exception: JSValueRef = null_mut();

    let result = unsafe {
        jsc_sys::JSEvaluateScript(
            self.ctx, script_ref, null_mut(), source_ref, 1, &mut exception
        )
    };

    // ALWAYS check exception
    if !exception.is_null() {
        return Err(extract_exception(self.ctx, exception));
    }

    if result.is_null() {
        return Err(JscError::Internal("Eval returned null without exception".into()));
    }

    Ok(unsafe { JscValue::new(self.ctx, result) })
}
```

### Extract Structured Errors

Convert JS exceptions to structured Rust errors:

```rust
pub(crate) unsafe fn extract_exception(ctx: JSContextRef, exception: JSValueRef) -> JscError {
    let mut ex: JSValueRef = null_mut();
    let js_str = jsc_sys::JSValueToStringCopy(ctx, exception, &mut ex);

    if js_str.is_null() {
        return JscError::script_error("Unknown error");
    }

    let message = js_string_to_rust(js_str);
    jsc_sys::JSStringRelease(js_str);

    // TODO: Extract line, column, stack from exception object
    JscError::ScriptError { message, line: None, column: None }
}
```

## Review Checklist

Before merging any PR with unsafe code:

- [ ] Is the unsafe block necessary? Can it be avoided?
- [ ] Is there a `// SAFETY:` comment explaining invariants?
- [ ] Are all pointers checked for null before dereference?
- [ ] Are string lifetimes correct (CString lives long enough)?
- [ ] Is memory freed (JSStringRelease, etc.) on all paths including errors?
- [ ] Are values protected before being stored across calls?
- [ ] Is the function thread-safe or properly documented as !Send?
- [ ] Are exceptions checked after every JSC API call?
- [ ] Does Drop implementation unprotect all protected values?
- [ ] Are there tests covering error paths?

## Common Pitfalls

### 1. CString Lifetime

```rust
// BAD: CString dropped before use
let ptr = CString::new(s).unwrap().as_ptr();  // CString dropped here!
unsafe { jsc_sys::JSStringCreateWithUTF8CString(ptr) }  // Use after free!

// GOOD: Keep CString alive
let cstr = CString::new(s).unwrap();
let js_str = unsafe { jsc_sys::JSStringCreateWithUTF8CString(cstr.as_ptr()) };
// cstr still alive here
```

### 2. Missing Null Check

```rust
// BAD: No null check
let value = unsafe { jsc_sys::JSValueMakeString(ctx, js_str) };
unsafe { jsc_sys::JSValueProtect(ctx, value) };  // Crash if value is null!

// GOOD: Check null
let value = unsafe { jsc_sys::JSValueMakeString(ctx, js_str) };
if value.is_null() {
    return Err(JscError::Internal("Failed to create string".into()));
}
```

### 3. Double Free

```rust
// BAD: Double release
let js_str = unsafe { jsc_sys::JSStringCreateWithUTF8CString(ptr) };
unsafe { jsc_sys::JSStringRelease(js_str) };
// ... later ...
unsafe { jsc_sys::JSStringRelease(js_str) };  // Double free!

// GOOD: Clear pointer after release, or use RAII wrapper
```

### 4. Forgetting to Unprotect

```rust
// BAD: Protect without matching unprotect = memory leak
unsafe { jsc_sys::JSValueProtect(ctx, value) };
// Forgot to unprotect!

// GOOD: Use RAII (JscValue wrapper handles this)
let value = unsafe { JscValue::new(ctx, raw_value) };
// Unprotect happens in Drop
```
