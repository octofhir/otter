/**
 * Memory allocation benchmark - tests object and array allocation performance.
 *
 * Run with:
 *   otter run benchmarks/memory/allocation.ts
 *   node --experimental-strip-types benchmarks/memory/allocation.ts
 *   bun benchmarks/memory/allocation.ts
 */

// Polyfill for performance.now() if not available
const now = typeof performance !== "undefined"
    ? () => performance.now()
    : () => Date.now();

function benchmark(name: string, fn: () => any, iterations: number = 1): number {
    // Warmup
    fn();

    const start = now();
    let result: any;
    for (let i = 0; i < iterations; i++) {
        result = fn();
    }
    const end = now();
    const duration = end - start;
    console.log(`${name}: ${duration.toFixed(2)}ms`);

    // Prevent optimization
    if (result === undefined) console.log("impossible");

    return duration;
}

function formatBytes(bytes: number): string {
    if (bytes < 1024) return `${bytes} B`;
    if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(2)} KB`;
    return `${(bytes / 1024 / 1024).toFixed(2)} MB`;
}

console.log("Memory Allocation Benchmark");
console.log("=".repeat(50));

// Object allocation
console.log("\nObject Allocation:");
benchmark("  10K small objects", () => {
    const arr: any[] = [];
    for (let i = 0; i < 10_000; i++) {
        arr.push({ id: i, name: `item${i}` });
    }
    return arr;
});

benchmark("  100K small objects", () => {
    const arr: any[] = [];
    for (let i = 0; i < 100_000; i++) {
        arr.push({ id: i, name: `item${i}` });
    }
    return arr;
});

benchmark("  10K medium objects", () => {
    const arr: any[] = [];
    for (let i = 0; i < 10_000; i++) {
        arr.push({
            id: i,
            name: `item${i}`,
            data: new Array(10).fill(i),
            metadata: { created: Date.now(), tags: ["a", "b", "c"] },
        });
    }
    return arr;
});

// Array allocation
console.log("\nArray Allocation:");
benchmark("  1K arrays of 1K elements", () => {
    const arr: number[][] = [];
    for (let i = 0; i < 1_000; i++) {
        arr.push(new Array(1_000).fill(i));
    }
    return arr;
});

benchmark("  10K arrays of 100 elements", () => {
    const arr: number[][] = [];
    for (let i = 0; i < 10_000; i++) {
        arr.push(new Array(100).fill(i));
    }
    return arr;
});

// TypedArray allocation
console.log("\nTypedArray Allocation:");
benchmark("  100 Uint8Array(1MB)", () => {
    const arr: Uint8Array[] = [];
    for (let i = 0; i < 100; i++) {
        arr.push(new Uint8Array(1024 * 1024));
    }
    return arr;
});

benchmark("  1000 Uint8Array(10KB)", () => {
    const arr: Uint8Array[] = [];
    for (let i = 0; i < 1000; i++) {
        arr.push(new Uint8Array(10 * 1024));
    }
    return arr;
});

benchmark("  10K Float64Array(1K)", () => {
    const arr: Float64Array[] = [];
    for (let i = 0; i < 10_000; i++) {
        arr.push(new Float64Array(1024));
    }
    return arr;
});

// String allocation
console.log("\nString Allocation:");
benchmark("  String concat (10K iterations)", () => {
    let str = "";
    for (let i = 0; i < 10_000; i++) {
        str += `item-${i}-`;
    }
    return str;
});

benchmark("  Array join (100K elements)", () => {
    const arr = Array.from({ length: 100_000 }, (_, i) => `item-${i}`);
    return arr.join("-");
});

// Map and Set
console.log("\nMap/Set Allocation:");
benchmark("  Map with 100K entries", () => {
    const map = new Map<number, string>();
    for (let i = 0; i < 100_000; i++) {
        map.set(i, `value-${i}`);
    }
    return map;
});

benchmark("  Set with 100K entries", () => {
    const set = new Set<number>();
    for (let i = 0; i < 100_000; i++) {
        set.add(i);
    }
    return set;
});

// Memory usage (if available)
if (typeof process !== "undefined" && process.memoryUsage) {
    console.log("\nMemory Usage:");
    const usage = process.memoryUsage();
    console.log(`  RSS: ${formatBytes(usage.rss)}`);
    console.log(`  Heap Total: ${formatBytes(usage.heapTotal)}`);
    console.log(`  Heap Used: ${formatBytes(usage.heapUsed)}`);
    console.log(`  External: ${formatBytes(usage.external)}`);
}

console.log("\nDone!");
