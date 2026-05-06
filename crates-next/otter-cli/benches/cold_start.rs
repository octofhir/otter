//! Criterion ratchets for CLI process cold-start paths.
//!
//! Each iteration spawns the compiled `otter` binary. These benches are more
//! environment-sensitive than in-process runtime benches, so Task 98 documents
//! the exact sample settings alongside the local results.

use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};

fn otter_binary() -> PathBuf {
    if let Some(path) = option_env!("CARGO_BIN_EXE_otter") {
        return PathBuf::from(path);
    }
    let mut path = std::env::current_exe().expect("bench exe path");
    path.pop();
    if path.ends_with("deps") {
        path.pop();
    }
    path.push(format!("otter{}", std::env::consts::EXE_SUFFIX));
    path
}

fn run_otter(args: &[&str]) {
    let status = Command::new(otter_binary())
        .args(args)
        .status()
        .expect("spawn otter");
    assert!(status.success(), "otter exited with {status}");
}

fn tiny_file(extension: &str, source: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("otter-startup-bench.{extension}"));
    std::fs::write(&path, source).expect("write tiny startup bench file");
    path
}

fn bench_cli_cold_start(c: &mut Criterion) {
    let tiny_js = tiny_file("js", "undefined;\n");
    let tiny_ts = tiny_file("ts", "const x: number = 1; x;\n");

    let mut group = c.benchmark_group("cli_cold_start");
    group.sample_size(10);
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(2));

    group.bench_function("eval_empty", |b| {
        b.iter_batched(|| (), |()| run_otter(&["-e", ""]), BatchSize::SmallInput);
    });
    group.bench_function("tiny_js_file", |b| {
        b.iter_batched(
            || tiny_js.clone(),
            |path| run_otter(&[path.to_str().expect("utf-8 path")]),
            BatchSize::SmallInput,
        );
    });
    group.bench_function("tiny_ts_file", |b| {
        b.iter_batched(
            || tiny_ts.clone(),
            |path| run_otter(&[path.to_str().expect("utf-8 path")]),
            BatchSize::SmallInput,
        );
    });
    group.finish();
}

criterion_group!(benches, bench_cli_cold_start);
criterion_main!(benches);
