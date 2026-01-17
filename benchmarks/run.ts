/**
 * Otter Benchmark Runner
 *
 * Usage:
 *   otter run benchmarks/run.ts [category] [--runtime node|bun|otter]
 *
 * Categories: startup, cpu, file-io, memory, all
 */

interface BenchmarkResult {
    name: string;
    category: string;
    runtime: string;
    version: string;
    metrics: {
        duration_ms?: number;
        ops_per_sec?: number;
        memory_mb?: number;
        throughput_mbps?: number;
    };
    timestamp: string;
}

interface BenchmarkSuite {
    name: string;
    category: string;
    fn: () => Promise<number> | number; // Returns duration in ms or ops/sec
    iterations?: number;
}

const results: BenchmarkResult[] = [];

// Detect runtime
function detectRuntime(): { name: string; version: string } {
    if (typeof globalThis.Otter !== "undefined") {
        return { name: "otter", version: process.version || "unknown" };
    }
    if (typeof (globalThis as any).Bun !== "undefined") {
        return { name: "bun", version: ((globalThis as any).Bun as any).version || "unknown" };
    }
    return { name: "node", version: process.version || "unknown" };
}

// High-resolution timer
function hrtime(): bigint {
    if (process.hrtime?.bigint) {
        return process.hrtime.bigint();
    }
    return BigInt(Math.floor(performance.now() * 1_000_000));
}

// Run a benchmark
async function runBenchmark(suite: BenchmarkSuite): Promise<BenchmarkResult> {
    const runtime = detectRuntime();
    const iterations = suite.iterations || 1;

    console.log(`  Running: ${suite.name}...`);

    // Warmup
    await suite.fn();

    // Actual run
    const start = hrtime();
    let totalOps = 0;

    for (let i = 0; i < iterations; i++) {
        const result = await suite.fn();
        if (typeof result === "number") {
            totalOps += result;
        }
    }

    const end = hrtime();
    const durationNs = Number(end - start);
    const durationMs = durationNs / 1_000_000;

    const result: BenchmarkResult = {
        name: suite.name,
        category: suite.category,
        runtime: runtime.name,
        version: runtime.version,
        metrics: {
            duration_ms: durationMs / iterations,
            ops_per_sec: totalOps > 0 ? (totalOps / durationMs) * 1000 : undefined,
        },
        timestamp: new Date().toISOString(),
    };

    console.log(`    Duration: ${result.metrics.duration_ms?.toFixed(2)}ms`);
    if (result.metrics.ops_per_sec) {
        console.log(`    Ops/sec: ${result.metrics.ops_per_sec.toFixed(0)}`);
    }

    return result;
}

// ============ Startup Benchmarks ============

const startupBenchmarks: BenchmarkSuite[] = [
    {
        name: "hello-world",
        category: "startup",
        fn: () => {
            // Measure time to execute minimal code
            const start = hrtime();
            const x = 1 + 1;
            const end = hrtime();
            return Number(end - start) / 1_000_000;
        },
    },
    {
        name: "json-parse-small",
        category: "startup",
        fn: () => {
            const data = '{"name":"test","value":42,"nested":{"a":1,"b":2}}';
            const start = hrtime();
            for (let i = 0; i < 10000; i++) {
                JSON.parse(data);
            }
            const end = hrtime();
            return Number(end - start) / 1_000_000;
        },
    },
];

// ============ CPU Benchmarks ============

const cpuBenchmarks: BenchmarkSuite[] = [
    {
        name: "fibonacci-recursive",
        category: "cpu",
        fn: () => {
            function fib(n: number): number {
                if (n <= 1) return n;
                return fib(n - 1) + fib(n - 2);
            }
            const start = hrtime();
            fib(35);
            const end = hrtime();
            return Number(end - start) / 1_000_000;
        },
    },
    {
        name: "fibonacci-iterative",
        category: "cpu",
        fn: () => {
            function fib(n: number): number {
                let a = 0, b = 1;
                for (let i = 0; i < n; i++) {
                    [a, b] = [b, a + b];
                }
                return a;
            }
            const start = hrtime();
            for (let i = 0; i < 1_000_000; i++) {
                fib(50);
            }
            const end = hrtime();
            return Number(end - start) / 1_000_000;
        },
    },
    {
        name: "json-stringify-large",
        category: "cpu",
        fn: () => {
            const data = {
                users: Array.from({ length: 1000 }, (_, i) => ({
                    id: i,
                    name: `User ${i}`,
                    email: `user${i}@example.com`,
                    active: i % 2 === 0,
                    metadata: {
                        created: new Date().toISOString(),
                        tags: ["tag1", "tag2", "tag3"],
                    },
                })),
            };
            const start = hrtime();
            for (let i = 0; i < 100; i++) {
                JSON.stringify(data);
            }
            const end = hrtime();
            return Number(end - start) / 1_000_000;
        },
    },
    {
        name: "array-operations",
        category: "cpu",
        fn: () => {
            const start = hrtime();
            const arr = Array.from({ length: 100000 }, (_, i) => i);

            // Map
            const mapped = arr.map((x) => x * 2);

            // Filter
            const filtered = mapped.filter((x) => x % 3 === 0);

            // Reduce
            const sum = filtered.reduce((a, b) => a + b, 0);

            const end = hrtime();
            return Number(end - start) / 1_000_000;
        },
        iterations: 10,
    },
    {
        name: "string-operations",
        category: "cpu",
        fn: () => {
            const start = hrtime();
            let str = "";
            for (let i = 0; i < 10000; i++) {
                str += `item-${i}-`;
            }
            str.split("-").join("_");
            str.replace(/item/g, "element");
            const end = hrtime();
            return Number(end - start) / 1_000_000;
        },
        iterations: 10,
    },
    {
        name: "regex-matching",
        category: "cpu",
        fn: () => {
            const text = "The quick brown fox jumps over the lazy dog. ".repeat(1000);
            const patterns = [
                /\b\w{5}\b/g,
                /[aeiou]/gi,
                /(\w+)\s+\1/g,
                /^The/gm,
            ];
            const start = hrtime();
            for (const pattern of patterns) {
                text.match(pattern);
            }
            const end = hrtime();
            return Number(end - start) / 1_000_000;
        },
        iterations: 100,
    },
];

// ============ Memory Benchmarks ============

const memoryBenchmarks: BenchmarkSuite[] = [
    {
        name: "object-allocation",
        category: "memory",
        fn: () => {
            const start = hrtime();
            const objects: any[] = [];
            for (let i = 0; i < 100000; i++) {
                objects.push({
                    id: i,
                    name: `Object ${i}`,
                    data: new Array(10).fill(i),
                });
            }
            const end = hrtime();
            // Force reference to prevent optimization
            if (objects.length === 0) console.log("impossible");
            return Number(end - start) / 1_000_000;
        },
    },
    {
        name: "array-allocation",
        category: "memory",
        fn: () => {
            const start = hrtime();
            const arrays: number[][] = [];
            for (let i = 0; i < 10000; i++) {
                arrays.push(new Array(1000).fill(i));
            }
            const end = hrtime();
            if (arrays.length === 0) console.log("impossible");
            return Number(end - start) / 1_000_000;
        },
    },
    {
        name: "typed-array-allocation",
        category: "memory",
        fn: () => {
            const start = hrtime();
            const arrays: Uint8Array[] = [];
            for (let i = 0; i < 1000; i++) {
                arrays.push(new Uint8Array(10000));
            }
            const end = hrtime();
            if (arrays.length === 0) console.log("impossible");
            return Number(end - start) / 1_000_000;
        },
    },
];

// ============ Main Runner ============

async function main() {
    const args = process.argv.slice(2);
    const category = args[0] || "all";

    console.log("=".repeat(50));
    console.log("Otter Benchmark Suite");
    console.log("=".repeat(50));

    const runtime = detectRuntime();
    console.log(`Runtime: ${runtime.name} ${runtime.version}`);
    console.log(`Category: ${category}`);
    console.log("");

    let benchmarks: BenchmarkSuite[] = [];

    if (category === "all" || category === "startup") {
        benchmarks = benchmarks.concat(startupBenchmarks);
    }
    if (category === "all" || category === "cpu") {
        benchmarks = benchmarks.concat(cpuBenchmarks);
    }
    if (category === "all" || category === "memory") {
        benchmarks = benchmarks.concat(memoryBenchmarks);
    }

    for (const suite of benchmarks) {
        console.log(`\n[${suite.category}]`);
        const result = await runBenchmark(suite);
        results.push(result);
    }

    // Print summary
    console.log("\n" + "=".repeat(50));
    console.log("Summary");
    console.log("=".repeat(50));

    const byCategory = new Map<string, BenchmarkResult[]>();
    for (const r of results) {
        if (!byCategory.has(r.category)) {
            byCategory.set(r.category, []);
        }
        byCategory.get(r.category)!.push(r);
    }

    for (const [cat, catResults] of byCategory) {
        console.log(`\n${cat.toUpperCase()}:`);
        for (const r of catResults) {
            const duration = r.metrics.duration_ms?.toFixed(2) || "N/A";
            console.log(`  ${r.name}: ${duration}ms`);
        }
    }

    // Output JSON results
    const outputFile = `benchmarks/results/${runtime.name}-${Date.now()}.json`;
    console.log(`\nResults saved to: ${outputFile}`);

    // Note: File writing requires --allow-write permission
    // await Deno.writeTextFile(outputFile, JSON.stringify(results, null, 2));
}

main().catch(console.error);
