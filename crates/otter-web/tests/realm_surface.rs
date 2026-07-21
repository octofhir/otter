//! Web extension installation must be realm-complete.
//!
//! Additional realms receive the same native classes, function globals and JS
//! extension surface as the default realm through the high-level runtime API.

use otter_runtime::{Runtime, SourceInput};
use otter_web::WebApiBuilderExt;

#[test]
fn additional_realm_receives_configured_web_extension() {
    let mut runtime = Runtime::builder()
        .with_web_apis()
        .build()
        .expect("web runtime");
    let realm = runtime.create_realm().expect("realm");
    let result = runtime
        .run_script_in_realm(
            realm,
            SourceInput::from_javascript(
                "[typeof URL, typeof fetch, typeof EventTarget, typeof structuredClone].join(':')",
            ),
            "realm:web-surface",
        )
        .expect("web realm");
    assert_eq!(
        result.completion_string(),
        "function:function:function:function"
    );
}
