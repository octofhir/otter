//! Worker-scoped capability narrowing (E1).
//!
//! A worker request can only shrink the parent's capability set: the
//! narrowed permission allows an operation only when both the parent
//! rule set and the requested subset allow it, and a requested
//! `AllowAll`/missing class inherits the parent set unchanged.

use std::path::PathBuf;

use otter_runtime::{CapabilitySet, Otter, Permission};

#[test]
fn narrowed_permission_allows_only_the_intersection() {
    let parent: Permission<String> =
        Permission::allow(["api.example.com".to_string(), "cdn.example.com".to_string()]);

    // Requested subset: only what both allow passes.
    let narrowed = parent.narrowed(Permission::allow(["api.example.com".to_string()]));
    assert!(narrowed.matches("api.example.com"));
    assert!(!narrowed.matches("cdn.example.com"));

    // A requested pattern outside the parent set cannot escalate.
    let escalating = parent.narrowed(Permission::allow(["evil.example.com".to_string()]));
    assert!(!escalating.matches("evil.example.com"));
    assert!(!escalating.matches("api.example.com"));

    // AllowAll is the identity in both directions.
    let inherited = parent.narrowed(Permission::AllowAll);
    assert!(inherited.matches("cdn.example.com"));
    let from_all = Permission::AllowAll.narrowed(Permission::allow(["x".to_string()]));
    assert!(from_all.matches("x"));
    assert!(!from_all.matches("y"));

    // Deny absorbs from either side.
    assert!(parent.narrowed(Permission::Deny).is_deny());
    assert!(
        Permission::<String>::Deny
            .narrowed(Permission::AllowAll)
            .is_deny()
    );

    // A deny pattern in either layer wins inside the intersection.
    let parent_with_deny: Permission<String> = Permission::allow_except(
        ["*.example.com".to_string()],
        ["admin.example.com".to_string()],
    );
    let narrowed = parent_with_deny.narrowed(Permission::allow([
        "admin.example.com".to_string(),
        "api.example.com".to_string(),
    ]));
    assert!(narrowed.matches("api.example.com"));
    assert!(!narrowed.matches("admin.example.com"));
}

#[test]
fn narrowed_path_permission_is_bounded_by_the_parent_prefixes() {
    let parent: Permission<PathBuf> = Permission::allow([PathBuf::from("/var/data")]);
    let narrowed = parent.narrowed(Permission::allow([PathBuf::from("/var/data/reports")]));
    assert!(narrowed.matches_path(std::path::Path::new("/var/data/reports/q1.json")));
    assert!(!narrowed.matches_path(std::path::Path::new("/var/data/secrets.json")));

    let escalating = parent.narrowed(Permission::allow([PathBuf::from("/etc")]));
    assert!(!escalating.matches_path(std::path::Path::new("/etc/passwd")));
}

#[test]
fn capability_set_narrows_every_class() {
    let parent = CapabilitySet {
        net: Permission::allow(["api.example.com".to_string()]),
        ..CapabilitySet::allow_all()
    };
    let narrowed = parent.narrowed(CapabilitySet {
        net: Permission::AllowAll,
        run: Permission::Deny,
        ..CapabilitySet::allow_all()
    });
    assert!(narrowed.net.matches("api.example.com"));
    assert!(!narrowed.net.matches("other.example.com"));
    assert!(narrowed.run.is_deny());
    assert!(narrowed.read.is_allow_all());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn worker_capability_request_narrows_the_child_runtime() {
    let dir = tempfile::tempdir().unwrap();
    let worker_path = dir.path().join("worker.js");
    std::fs::write(
        &worker_path,
        r#"
        import("https://net-narrowing.invalid/mod.js").then(
          () => postMessage("allowed"),
          (error) => postMessage(String((error && error.message) || error)),
        );
        "#,
    )
    .unwrap();
    let entry = dir.path().join("entry.js");
    std::fs::write(
        &entry,
        format!(
            r#"
            let got = "pending";
            const w = new Worker({:?}, {{ otter: {{ capabilities: {{ net: false }} }} }});
            w.onmessage = (event) => {{
              got = event.data;
              w.terminate();
            }};
            w.onerror = (event) => {{
              got = "ERR:" + event.message;
              w.terminate();
            }};
            setTimeout(() => {{
              if (!String(got).toLowerCase().includes("denied")) {{
                throw "expected a capability denial, got: " + got;
              }}
            }}, 50);
            "#,
            worker_path.to_string_lossy()
        ),
    )
    .unwrap();

    let otter = Otter::builder()
        .capabilities(CapabilitySet::allow_all())
        .build()
        .unwrap();
    otter.run_file(&entry).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn worker_capability_request_cannot_escalate_past_the_parent() {
    let dir = tempfile::tempdir().unwrap();
    let worker_path = dir.path().join("worker.js");
    std::fs::write(
        &worker_path,
        r#"
        import("https://blocked.invalid/mod.js").then(
          () => postMessage("allowed"),
          (error) => postMessage(String((error && error.message) || error)),
        );
        "#,
    )
    .unwrap();
    let entry = dir.path().join("entry.js");
    std::fs::write(
        &entry,
        format!(
            r#"
            let got = "pending";
            const w = new Worker({:?}, {{
              otter: {{ capabilities: {{ net: ["blocked.invalid"] }} }},
            }});
            w.onmessage = (event) => {{
              got = event.data;
              w.terminate();
            }};
            w.onerror = (event) => {{
              got = "ERR:" + event.message;
              w.terminate();
            }};
            setTimeout(() => {{
              if (!String(got).toLowerCase().includes("denied")) {{
                throw "requested host outside the parent set must stay denied: " + got;
              }}
            }}, 50);
            "#,
            worker_path.to_string_lossy()
        ),
    )
    .unwrap();

    // Parent allows only the API host; the worker asks for a different
    // host, and the intersection denies it.
    let parent = CapabilitySet {
        net: Permission::allow(["api.example.com".to_string()]),
        ..CapabilitySet::allow_all()
    };
    let otter = Otter::builder().capabilities(parent).build().unwrap();
    otter.run_file(&entry).await.unwrap();
}
