//! Fresh-process startup-image admission, cage relocation and collecting reuse.
//!
//! # Contents
//! - Image capture in a copy of this exact test executable.
//! - Raw/restored child launches in every ordinary execution policy.
//! - Weak bodies, exact primitive values and post-restore full collection.
//!
//! # Invariants
//! - A restored child must report actual image admission; bootstrap fallback
//!   cannot satisfy the proof.
//! - Capture and launch use the same surface and linker build identity.
//! - Only test-owned temporary copies are patched and, on macOS, re-signed.
//! - Child processes use their default Rust stacks and independent cage bases.
//!
//! # See also
//! - `otter-gc/src/heap_image.rs` owns page and cage-address relocation.

use std::io::{Read, Seek};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use otter_runtime::{JitSelection, SourceInput};

const ROLE: &str = "OTTER_SNAPSHOT_PROCESS_ROLE";
const IMAGE: &str = "OTTER_SNAPSHOT_PROCESS_IMAGE";
const TIER: &str = "OTTER_SNAPSHOT_PROCESS_TIER";
const TEST: &str = "startup_image_process::restored_process_uses_the_image_and_survives_gc";
const SEED: &str = r#"
globalThis.imageKept = [{v:719},{v:817}];
globalThis.imageWeakMap = new WeakMap(imageKept.map(key => [key, {v:key.v}]));
globalThis.imageWeakRef = new WeakRef(imageKept[0]);
globalThis.imageText = '\ud800image\udc00';
globalThis.imageNumbers = [-0, NaN, Number.MIN_VALUE];
"#;
const PROBE: &str = r#"
JSON.stringify([imageKept.length, imageWeakMap.get(imageKept[0]).v,
    imageWeakRef.deref() === imageKept[0], imageText.charCodeAt(0),
    Object.is(imageNumbers[0], -0), Number.isNaN(imageNumbers[1]),
    imageNumbers[2] === Number.MIN_VALUE,
    imageDoubles[0] === Number(imageDoubleText), new Function('return 42')()]);
"#;

fn child_command(executable: &std::path::Path, role: &str) -> Command {
    let mut command = Command::new(executable);
    command
        .args(["--exact", TEST, "--nocapture"])
        .env(ROLE, role)
        .env_remove("RUST_MIN_STACK");
    command
}

fn check_child(output: Output, label: &str) {
    assert!(
        output.status.success(),
        "{label}: {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    for line in String::from_utf8_lossy(&output.stderr).lines() {
        if line.starts_with("image-process:") {
            eprintln!("{line}");
        }
    }
}

fn bounded_output(command: &mut Command) -> Output {
    let mut stdout = tempfile::tempfile().expect("child stdout");
    let mut stderr = tempfile::tempfile().expect("child stderr");
    let mut child = command
        .stdout(Stdio::from(stdout.try_clone().unwrap()))
        .stderr(Stdio::from(stderr.try_clone().unwrap()))
        .spawn()
        .expect("spawn child");
    let started = Instant::now();
    let mut timed_out = false;
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll child") {
            break status;
        }
        if started.elapsed() >= Duration::from_secs(60) {
            timed_out = true;
            child.kill().expect("kill timed-out child");
            break child.wait().expect("reap timed-out child");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    stdout.rewind().unwrap();
    stderr.rewind().unwrap();
    let mut out = Vec::new();
    let mut err = Vec::new();
    stdout.read_to_end(&mut out).unwrap();
    stderr.read_to_end(&mut err).unwrap();
    assert!(
        !timed_out,
        "child timed out; stdout: {}; stderr: {}",
        String::from_utf8_lossy(&out),
        String::from_utf8_lossy(&err)
    );
    Output {
        status,
        stdout: out,
        stderr: err,
    }
}

fn run_child(role: &str) {
    let selection = match std::env::var(TIER).as_deref() {
        Ok("interpreter") => JitSelection::InterpreterOnly,
        Ok("template") => JitSelection::Template,
        _ => JitSelection::ProductionTiered,
    };
    let mut runtime = super::build_runtime(
        super::full_surface_builder()
            .startup_image(true)
            .jit_selection(selection),
    );
    assert_eq!(runtime.restored_from_snapshot(), role == "restored");
    eprintln!(
        "image-process: {role} {selection:?} cage={:p} restored={}",
        otter_gc::cage_base(),
        runtime.restored_from_snapshot()
    );
    if !runtime.restored_from_snapshot() {
        runtime
            .run_script(SourceInput::from_javascript(SEED), "image-seed.js")
            .expect("seed capture graph");
        // An unboxed double can have the same bytes as a cage address. Only
        // typed Value slots may be rebased, never arbitrary matching words.
        let value = f64::from_bits(otter_gc::cage_base() as u64 | 0x4_0040);
        runtime
            .run_script(
                SourceInput::from_javascript(format!(
                    "globalThis.imageDoubles = [{value:e}]; globalThis.imageDoubleText = '{value:e}';"
                )),
                "image-double.js",
            )
            .expect("seed address-shaped numeric storage");
        runtime.force_gc().expect("tenure seed graph");
    }
    if role == "capture" {
        runtime.prepare_startup_image().expect("prepare console");
        runtime
            .force_gc()
            .expect("tenure the complete capture graph");
        let bytes = runtime.encode_startup_image().expect("encode own image");
        std::fs::write(std::env::var_os(IMAGE).expect("image path"), bytes)
            .expect("write captured image");
        return;
    }
    if runtime.restored_from_snapshot() {
        // Restore deliberately clears captured WeakRef targets. Check that
        // boundary exactly, then prove a new reference joins the weak registry.
        assert_eq!(
            runtime
                .eval(SourceInput::from_javascript(
                    "imageWeakRef.deref() === undefined"
                ))
                .expect("captured weak target is cleared")
                .completion_string(),
            "true"
        );
        runtime
            .run_script(
                SourceInput::from_javascript("imageWeakRef = new WeakRef(imageKept[0]);"),
                "image-new-weak-ref.js",
            )
            .expect("register a fresh restored-process weak reference");
    }
    for _ in 0..3 {
        runtime
            .run_script(
                SourceInput::from_javascript(
                    "var imageJunk; for(var i=0;i<10000;i++){imageJunk={a:i,b:[i,i+1]};}",
                ),
                "image-pressure.js",
            )
            .expect("restored allocation pressure");
        runtime.force_gc().expect("full collection in child");
        let result = runtime
            .run_script(SourceInput::from_javascript(PROBE), "image-probe.js")
            .expect("probe captured values and weak bodies");
        assert_eq!(
            result.completion_string(),
            "[2,719,true,55296,true,true,true,true,42]"
        );
    }
}

#[test]
fn restored_process_uses_the_image_and_survives_gc() {
    if let Ok(role) = std::env::var(ROLE) {
        run_child(&role);
        return;
    }
    let directory = tempfile::tempdir().expect("private executable directory");
    let raw = directory
        .path()
        .join(format!("raw{}", std::env::consts::EXE_SUFFIX));
    let restored = directory
        .path()
        .join(format!("restored{}", std::env::consts::EXE_SUFFIX));
    let image = directory.path().join("image.bin");
    let executable = std::env::current_exe().expect("current executable");
    std::fs::copy(&executable, &raw).expect("copy raw executable");
    std::fs::copy(&executable, &restored).expect("copy image executable");
    check_child(
        bounded_output(child_command(&restored, "capture").env(IMAGE, &image)),
        "capture",
    );
    otter_vm::startup_image::write_into_executable(
        &restored,
        &std::fs::read(image).expect("captured bytes"),
    )
    .expect("patch only private executable copy");
    #[cfg(target_vendor = "apple")]
    check_child(
        bounded_output(
            Command::new("codesign")
                .args(["--force", "--sign", "-"])
                .arg(&restored),
        ),
        "codesign",
    );
    for tier in ["interpreter", "template", "production"] {
        for (path, role) in [(&raw, "raw"), (&restored, "restored")] {
            check_child(
                bounded_output(child_command(path, role).env(TIER, tier)),
                &format!("{role} {tier}"),
            );
        }
    }
}
