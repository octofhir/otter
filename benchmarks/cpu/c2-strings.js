// C2 String Hierarchy — runtime comparison (Otter / Node / Bun).
//
// Same workloads as crates/otter-vm/benches/c2_string_bench.rs, exercised
// at the JS layer. Each phase prints a single CSV line:
//   case,duration_ms,length
//
// Driven by benchmarks/c2-strings-compare.sh.

function ms() { return performance.now(); }

function csv(name, dur, len) {
    console.log(name + "," + dur.toFixed(3) + "," + len);
}

// ─── 1. += loop, 256 KB target ──────────────────────────────────────────────
{
    const chunk = "abcdefghijklmnopqrstuvwxyz0123456789"; // 36 B
    const target = 256 * 1024;
    const t0 = ms();
    let s = "";
    while (s.length < target) {
        s = s + chunk;
    }
    // Force observe — read length & charCodeAt(0) so any lazy repr flattens.
    const _ = s.charCodeAt(0);
    const t1 = ms();
    csv("concat_loop_256kb", t1 - t0, s.length);
}

// ─── 2. slice non-observed × 100k ───────────────────────────────────────────
{
    const big = "abcdefghij".repeat(1638); // ~16 KB
    const t0 = ms();
    let acc = 0;
    for (let i = 0; i < 100000; i++) {
        const off = i % 1000;
        const s = big.slice(off, off + 10);
        // Don't observe content — just count lengths so the loop can't be
        // dead-code eliminated. (Bun/Node still allocate the substring;
        // Otter post-C2 builds a Sliced view.)
        acc += s.length;
    }
    const t1 = ms();
    csv("slice_view_100k", t1 - t0, acc);
}

// ─── 3. slice observed × 10k ─────────────────────────────────────────────────
{
    const big = "abcdefghij".repeat(1638);
    const t0 = ms();
    let acc = 0;
    for (let i = 0; i < 10000; i++) {
        const off = i % 1000;
        const s = big.slice(off, off + 10);
        acc += s.charCodeAt(0); // forces flatten on Otter
    }
    const t1 = ms();
    csv("slice_observed_10k", t1 - t0, acc);
}

// ─── 4. ASCII alloc 1 MB ────────────────────────────────────────────────────
{
    const t0 = ms();
    const s = "abcdefghij".repeat(100000); // 1 MB
    const _ = s.charCodeAt(0);
    const t1 = ms();
    csv("ascii_alloc_1mb", t1 - t0, s.length);
}

// ─── 5. indexOf at end of 256 KB haystack ───────────────────────────────────
{
    const haystack = "abcdefghij".repeat(26214) + "FOUND"; // ~256 KB
    const t0 = ms();
    let pos = 0;
    for (let i = 0; i < 100; i++) {
        pos = haystack.indexOf("FOUND");
    }
    const t1 = ms();
    csv("index_of_256kb_x100", t1 - t0, pos);
}

// ─── 6. Property-key lookup × 1M ────────────────────────────────────────────
{
    // Build an object with 1000 string keys, then access each 1000 times.
    // Hash caching benefit on Otter; Node/Bun also cache.
    const obj = {};
    for (let i = 0; i < 1000; i++) {
        obj["propName_" + i.toString().padStart(8, "0")] = i;
    }
    const keys = Object.keys(obj);
    const t0 = ms();
    let sum = 0;
    for (let iter = 0; iter < 1000; iter++) {
        for (let i = 0; i < keys.length; i++) {
            sum += obj[keys[i]];
        }
    }
    const t1 = ms();
    csv("prop_lookup_1m", t1 - t0, sum);
}
