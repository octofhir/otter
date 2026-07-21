//! High-level additional-realm embedding coverage.
//!
//! # Invariants
//! - Public realm identity is an owned scalar, never a VM/GC handle.
//! - Configured installers run in every realm through the safe installer API.
//! - Globals are isolated while repeated turns in one realm retain state.
//! - Layer B routes realm operations through the owning isolate.

use otter_runtime::{
    OtterError, RealmError, Runtime, RuntimeGlobalInstaller, RuntimeRealmContext, RuntimeRealmId,
    SourceInput,
};

fn install_marker(realm: &mut RuntimeRealmContext<'_>) -> Result<(), otter_runtime::OtterError> {
    realm.install_global("hostName", "acme")?;
    realm.install_script(SourceInput::from_javascript(
        "globalThis.hostInstallCount = (globalThis.hostInstallCount || 0) + 1;",
    ))
}

fn assert_send_sync<T: Send + Sync + 'static>() {}

#[test]
fn realm_id_is_an_owned_send_sync_identity() {
    assert_send_sync::<RuntimeRealmId>();
}

#[test]
fn direct_runtime_realms_are_bootstrapped_isolated_and_reusable() {
    let mut runtime = Runtime::builder()
        .global_installer(RuntimeGlobalInstaller::new(install_marker))
        .build()
        .expect("runtime");
    let realm = runtime.create_realm().expect("realm");

    let first = runtime
        .run_script_in_realm(
            realm,
            SourceInput::from_javascript(
                "let realmLexical = 7; globalThis.realmCounter = (globalThis.realmCounter || 0) + 1; hostName + ':' + hostInstallCount;",
            ),
            "realm:first",
        )
        .expect("first turn");
    assert_eq!(first.completion_string(), "acme:1");

    let second = runtime
        .run_script_in_realm(
            realm,
            SourceInput::from_javascript("++globalThis.realmCounter + realmLexical"),
            "realm:second",
        )
        .expect("second turn");
    assert_eq!(second.completion_string(), "9");

    let default = runtime
        .run_script(
            SourceInput::from_javascript(
                "typeof realmCounter + ':' + typeof realmLexical + ':' + hostInstallCount",
            ),
            "default:check",
        )
        .expect("default realm");
    assert_eq!(default.completion_string(), "undefined:undefined:1");

    let other = runtime.create_realm().expect("second realm");
    let isolated = runtime
        .run_script_in_realm(
            other,
            SourceInput::from_javascript(
                "typeof realmCounter + ':' + typeof realmLexical + ':' + hostInstallCount",
            ),
            "realm:isolated",
        )
        .expect("isolated realm");
    assert_eq!(isolated.completion_string(), "undefined:undefined:1");
}

#[test]
fn realm_ids_reject_cross_runtime_use_and_disposal_is_final() {
    let mut first = Runtime::builder().build().expect("first runtime");
    let realm = first.create_realm().expect("first realm");

    let mut second = Runtime::builder().build().expect("second runtime");
    let _colliding_local_id = second.create_realm().expect("second realm");
    let wrong_runtime = second
        .run_script_in_realm(
            realm,
            SourceInput::from_javascript("1"),
            "realm:wrong-runtime",
        )
        .expect_err("foreign realm id must be rejected");
    assert!(matches!(
        wrong_runtime,
        OtterError::Realm {
            reason: RealmError::WrongRuntime
        }
    ));

    first.dispose_realm(realm).expect("dispose realm");
    let stale = first
        .run_script_in_realm(realm, SourceInput::from_javascript("1"), "realm:disposed")
        .expect_err("disposed realm id must be rejected");
    assert!(matches!(
        stale,
        OtterError::Realm {
            reason: RealmError::UnknownOrDisposed
        }
    ));
    assert!(matches!(
        first.dispose_realm(realm),
        Err(OtterError::Realm {
            reason: RealmError::UnknownOrDisposed
        })
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runtime_handle_exposes_only_high_level_realm_operations() {
    let handle = Runtime::builder()
        .global_installer(RuntimeGlobalInstaller::new(install_marker))
        .build_handle()
        .expect("handle");
    let realm = handle.create_realm().await.expect("realm");
    let result = handle
        .run_script_in_realm(
            realm,
            SourceInput::from_javascript("hostInstallCount + 41"),
            "realm:handle",
        )
        .await
        .expect("realm run");
    assert_eq!(result.completion_string(), "42");
    handle.dispose_realm(realm).await.expect("dispose realm");
    let stale = handle
        .run_script_in_realm(
            realm,
            SourceInput::from_javascript("1"),
            "realm:disposed-handle",
        )
        .await
        .expect_err("disposed realm must stay invalid");
    assert!(matches!(
        stale,
        OtterError::Realm {
            reason: RealmError::UnknownOrDisposed
        }
    ));
}
