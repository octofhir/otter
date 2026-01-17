/**
 * Fibonacci benchmark - tests recursive and iterative CPU performance.
 *
 * Run with:
 *   otter run benchmarks/cpu/fibonacci.ts
 *   node --experimental-strip-types benchmarks/cpu/fibonacci.ts
 *   bun benchmarks/cpu/fibonacci.ts
 */

// Polyfill for performance.now() if not available
const now = typeof performance !== "undefined"
    ? () => performance.now()
    : () => Date.now();

function fibRecursive(n: number): number {
    if (n <= 1) return n;
    return fibRecursive(n - 1) + fibRecursive(n - 2);
}

function fibIterative(n: number): number {
    let a = 0, b = 1;
    for (let i = 0; i < n; i++) {
        [a, b] = [b, a + b];
    }
    return a;
}

function benchmark(name: string, fn: () => void, iterations: number = 1): number {
    const start = now();
    for (let i = 0; i < iterations; i++) {
        fn();
    }
    const end = now();
    const duration = end - start;
    console.log(`${name}: ${duration.toFixed(2)}ms (${iterations} iterations)`);
    return duration;
}

console.log("Fibonacci Benchmark");
console.log("=".repeat(40));

// Recursive (compute-heavy)
benchmark("fib(35) recursive", () => fibRecursive(35));
benchmark("fib(38) recursive", () => fibRecursive(38));

// Iterative (loop-heavy)
benchmark("fib(50) iterative x1M", () => {
    for (let i = 0; i < 1_000_000; i++) {
        fibIterative(50);
    }
});

benchmark("fib(1000) iterative x100K", () => {
    for (let i = 0; i < 100_000; i++) {
        fibIterative(1000);
    }
});

console.log("\nDone!");
