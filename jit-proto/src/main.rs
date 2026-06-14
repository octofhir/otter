//! §3.2 backend prototype gate — Milestone 1: dynasm-rs arm64 toolchain sanity.
//!
//! Goal: prove we can emit + execute JIT machine code on this Apple-Silicon box
//! (macOS arm64 needs MAP_JIT + W^X toggling) and get the first two headline
//! numbers the gate cares about: compile latency and native ns/op. No otter
//! Value tagging / GC yet — that is Milestone 2.

use dynasmrt::{dynasm, DynasmApi, DynasmLabelApi};
use std::time::Instant;

/// Emit `fn() -> i32 { 42 }` — smallest possible toolchain sanity check.
fn build_ret42() -> dynasmrt::ExecutableBuffer {
    let mut ops = dynasmrt::aarch64::Assembler::new().unwrap();
    dynasm!(ops
        ; .arch aarch64
        ; movz w0, 42
        ; ret
    );
    ops.finalize().unwrap()
}

/// Emit a recursive `fn fib(i32) -> i32` in native arm64. Mirrors fib.js's
/// shape (compare, two self-calls, add) but on raw integers — the codegen
/// ceiling, not the tagged-Value baseline number.
fn build_fib() -> (dynasmrt::ExecutableBuffer, dynasmrt::AssemblyOffset) {
    let mut ops = dynasmrt::aarch64::Assembler::new().unwrap();
    let start = ops.offset();
    dynasm!(ops
        ; .arch aarch64
        ; ->fib:
        ; cmp w0, #2
        ; b.lt >done
        ; stp x29, x30, [sp, #-32]!     // save fp, lr
        ; stp x19, x20, [sp, #16]       // save callee-saved
        ; mov w19, w0                   // x19 = n
        ; sub w0, w0, #1
        ; bl ->fib                      // fib(n-1)
        ; mov w20, w0                   // x20 = fib(n-1)
        ; sub w0, w19, #2
        ; bl ->fib                      // fib(n-2)
        ; add w0, w0, w20               // fib(n-1)+fib(n-2)
        ; ldp x19, x20, [sp, #16]
        ; ldp x29, x30, [sp], #32
        ; ret
        ; done:
        ; ret                           // n < 2 -> return n
    );
    let buf = ops.finalize().unwrap();
    (buf, start)
}

#[inline(never)]
fn rust_fib(n: i32) -> i32 {
    if n < 2 { n } else { rust_fib(n - 1) + rust_fib(n - 2) }
}

// --- M2: tagged NaN-box Values (otter layout) ---
// top 16 bits = tag; TAG_INT32 = 0x7FF9; int32 payload in low 32 bits
// (value/tag.rs:46-86). box = (0x7FF9 << 48) | (i as u32); unbox = low32 as i32.
const TAG_INT32: u64 = 0x7FF9;
#[inline]
fn box_i32(v: i32) -> u64 { (TAG_INT32 << 48) | (v as u32 as u64) }
#[inline]
fn unbox_i32(v: u64) -> i32 { v as u32 as i32 }

/// Emit `fn fib_tagged(u64) -> u64` operating on tagged NaN-box Values:
/// int32 guard on entry (fast path; mistyped traps), checked arithmetic with
/// re-boxing, self-recursive compiled→compiled calls. Models the realistic
/// baseline-JIT per-op cost (guard + unbox + op + rebox), not the M1 native
/// ceiling. (The recursive call is direct compiled→compiled — the optimistic
/// case; a VM-invoke trampoline would add frame-setup cost on top.)
fn build_fib_tagged() -> (dynasmrt::ExecutableBuffer, dynasmrt::AssemblyOffset) {
    let mut ops = dynasmrt::aarch64::Assembler::new().unwrap();
    let start = ops.offset();
    dynasm!(ops
        ; .arch aarch64
        ; ->fibt:
        ; lsr x9, x0, #48               // int32 guard: tag = top 16 bits
        ; movz x10, #0x7ff9             //   (cmp imm is 12-bit; load tag to reg)
        ; cmp x9, x10
        ; b.ne >slow                    //   mistyped -> slow (never taken here)
        ; cmp w0, #2                    // n = low 32 bits
        ; b.lt >done                    //   n < 2 -> return tagged n unchanged
        ; stp x29, x30, [sp, #-48]!
        ; stp x19, x20, [sp, #16]
        ; stp x21, x22, [sp, #32]
        ; movz x21, #0x7ff9, lsl #48    // box constant (held)
        ; mov w19, w0                   // x19 = n
        ; sub w0, w19, #1               // n-1 (top zeroed by 32-bit op)
        ; orr x0, x0, x21               // rebox
        ; bl ->fibt                     // fib(n-1)
        ; mov w20, w0                   // x20 = unbox fib(n-1)
        ; sub w0, w19, #2               // n-2
        ; orr x0, x0, x21               // rebox
        ; bl ->fibt                     // fib(n-2)
        ; add w0, w0, w20               // fib(n-1)+fib(n-2)
        ; orr x0, x0, x21               // rebox result
        ; ldp x21, x22, [sp, #32]
        ; ldp x19, x20, [sp, #16]
        ; ldp x29, x30, [sp], #48
        ; ret
        ; done:
        ; ret
        ; slow:
        ; brk #1                        // proto: trap on type mismatch
    );
    let buf = ops.finalize().unwrap();
    (buf, start)
}

fn main() {
    // --- Sanity: emit + run `ret 42` ---
    let buf = build_ret42();
    let f: extern "C" fn() -> i32 = unsafe { std::mem::transmute(buf.ptr(dynasmrt::AssemblyOffset(0))) };
    let got = f();
    println!("[sanity] jit ret42 = {got} (expect 42)  -> toolchain {}",
        if got == 42 { "WORKS on darwin/arm64" } else { "BROKEN" });

    // --- Compile latency: assemble+finalize fib N times, report min/median µs ---
    const COMPILES: usize = 2000;
    let mut times_ns: Vec<u128> = Vec::with_capacity(COMPILES);
    for _ in 0..COMPILES {
        let t = Instant::now();
        let (b, _s) = build_fib();
        std::hint::black_box(&b);
        times_ns.push(t.elapsed().as_nanos());
    }
    times_ns.sort_unstable();
    let min_us = times_ns[0] as f64 / 1000.0;
    let med_us = times_ns[COMPILES / 2] as f64 / 1000.0;
    println!("[compile-latency] fib ({} ops emitted): min={:.3}µs  median={:.3}µs  ({} samples)",
        14, min_us, med_us, COMPILES);

    // --- ns/op: run jit fib vs rust-native fib ---
    let (fbuf, fstart) = build_fib();
    let jfib: extern "C" fn(i32) -> i32 = unsafe { std::mem::transmute(fbuf.ptr(fstart)) };
    let n = 31i32;

    // correctness
    let jv = jfib(n);
    let rv = rust_fib(n);
    println!("[correct] jit fib({n})={jv}  rust fib({n})={rv}  match={}", jv == rv);

    let calls = fib_calls(n) as u128; // number of fib invocations = work units

    let reps = 30u32;
    let mut jit_min = u128::MAX;
    for _ in 0..reps {
        let t = Instant::now();
        std::hint::black_box(jfib(n));
        jit_min = jit_min.min(t.elapsed().as_nanos());
    }
    let mut rust_min = u128::MAX;
    for _ in 0..reps {
        let t = Instant::now();
        std::hint::black_box(rust_fib(n));
        rust_min = rust_min.min(t.elapsed().as_nanos());
    }
    println!("[ns/op] fib({n}): jit={:.2}ns/call  rust-native={:.2}ns/call  (over {} calls, min of {} reps)",
        jit_min as f64 / calls as f64, rust_min as f64 / calls as f64, calls, reps);
    println!("[ns/op] jit total={:.3}ms  rust total={:.3}ms", jit_min as f64 / 1e6, rust_min as f64 / 1e6);

    // --- M2: tagged NaN-box fib (realistic baseline codegen) ---
    let (tbuf, tstart) = build_fib_tagged();
    let tfib: extern "C" fn(u64) -> u64 = unsafe { std::mem::transmute(tbuf.ptr(tstart)) };
    let tv = unbox_i32(tfib(box_i32(n)));
    println!("\n[M2 correct] tagged-jit fib({n})={tv}  match={}", tv == rv);
    let mut t_min = u128::MAX;
    for _ in 0..reps {
        let t = Instant::now();
        std::hint::black_box(tfib(std::hint::black_box(box_i32(n))));
        t_min = t_min.min(t.elapsed().as_nanos());
    }
    let interp_ns = 532.1f64; // otter fib.js 2328ms - 10ms startup over 4356617 calls
    let tagged_ns = t_min as f64 / calls as f64;
    println!("[M2 ns/op] tagged-jit={:.2}ns/call  native={:.2}ns/call  interp~={:.1}ns/call",
        tagged_ns, jit_min as f64 / calls as f64, interp_ns);
    println!("[M2 speedup] tagged-jit vs interp = {:.0}x faster   (tag overhead vs native = {:.1}x)",
        interp_ns / tagged_ns, tagged_ns / (jit_min as f64 / calls as f64));
    println!("[M2 ns/op] tagged-jit total={:.3}ms", t_min as f64 / 1e6);
}

/// Count of fib() invocations for fib(n) = 2*Fib(n+1) - 1.
fn fib_calls(n: i32) -> u64 {
    fn go(n: i32, memo: &mut std::collections::HashMap<i32, u64>) -> u64 {
        if n < 2 { return 1; }
        if let Some(&v) = memo.get(&n) { return v; }
        let v = 1 + go(n - 1, memo) + go(n - 2, memo);
        memo.insert(n, v);
        v
    }
    go(n, &mut std::collections::HashMap::new())
}
