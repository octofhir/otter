use crate::JscResult;
use crate::bindings::*;
use std::ffi::CString;
use std::ptr;

const BOOTSTRAP_JS: &str = include_str!("bootstrap.js");

pub fn register_bootstrap(ctx: JSContextRef) -> JscResult<()> {
    unsafe {
        let script_cstr = CString::new(BOOTSTRAP_JS).expect("BOOTSTRAP_JS contains null byte");
        let script_ref = JSStringCreateWithUTF8CString(script_cstr.as_ptr());

        let source_cstr = CString::new("<otter_bootstrap>").unwrap();
        let source_ref = JSStringCreateWithUTF8CString(source_cstr.as_ptr());

        let mut exception: JSValueRef = ptr::null_mut();
        JSEvaluateScript(
            ctx,
            script_ref,
            ptr::null_mut(),
            source_ref,
            1,
            &mut exception,
        );

        JSStringRelease(script_ref);
        JSStringRelease(source_ref);

        if !exception.is_null() {
            return Err(crate::value::extract_exception(ctx, exception).into());
        }
    }

    Ok(())
}
