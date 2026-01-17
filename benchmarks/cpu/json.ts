/**
 * JSON benchmark - tests JSON.parse and JSON.stringify performance.
 *
 * Run with:
 *   otter run benchmarks/cpu/json.ts
 *   node --experimental-strip-types benchmarks/cpu/json.ts
 *   bun benchmarks/cpu/json.ts
 */

// Polyfill for performance.now() if not available
const now = typeof performance !== "undefined"
    ? () => performance.now()
    : () => Date.now();

// Generate test data
function generateUsers(count: number) {
    return Array.from({ length: count }, (_, i) => ({
        id: i,
        name: `User ${i}`,
        email: `user${i}@example.com`,
        active: i % 2 === 0,
        age: 20 + (i % 50),
        metadata: {
            created: new Date().toISOString(),
            lastLogin: new Date().toISOString(),
            preferences: {
                theme: i % 2 === 0 ? "dark" : "light",
                notifications: true,
                language: "en",
            },
            tags: ["tag1", "tag2", "tag3"],
        },
    }));
}

function benchmark(name: string, fn: () => void, iterations: number): number {
    // Warmup
    fn();
    fn();

    const start = now();
    for (let i = 0; i < iterations; i++) {
        fn();
    }
    const end = now();
    const duration = end - start;
    const opsPerSec = (iterations / duration) * 1000;
    console.log(`${name}: ${duration.toFixed(2)}ms (${opsPerSec.toFixed(0)} ops/sec)`);
    return duration;
}

console.log("JSON Benchmark");
console.log("=".repeat(50));

// Small object
const smallObj = { name: "test", value: 42, active: true };
const smallJson = JSON.stringify(smallObj);

console.log("\nSmall Object (3 fields):");
benchmark("  JSON.parse", () => JSON.parse(smallJson), 100_000);
benchmark("  JSON.stringify", () => JSON.stringify(smallObj), 100_000);

// Medium object
const mediumObj = generateUsers(10)[0];
const mediumJson = JSON.stringify(mediumObj);

console.log("\nMedium Object (nested, ~20 fields):");
benchmark("  JSON.parse", () => JSON.parse(mediumJson), 50_000);
benchmark("  JSON.stringify", () => JSON.stringify(mediumObj), 50_000);

// Large array
const largeObj = { users: generateUsers(100) };
const largeJson = JSON.stringify(largeObj);

console.log("\nLarge Array (100 users):");
benchmark("  JSON.parse", () => JSON.parse(largeJson), 1_000);
benchmark("  JSON.stringify", () => JSON.stringify(largeObj), 1_000);

// Very large array
const veryLargeObj = { users: generateUsers(1000) };
const veryLargeJson = JSON.stringify(veryLargeObj);

console.log("\nVery Large Array (1000 users):");
benchmark("  JSON.parse", () => JSON.parse(veryLargeJson), 100);
benchmark("  JSON.stringify", () => JSON.stringify(veryLargeObj), 100);

console.log("\nData sizes:");
console.log(`  Small: ${smallJson.length} bytes`);
console.log(`  Medium: ${mediumJson.length} bytes`);
console.log(`  Large: ${largeJson.length} bytes`);
console.log(`  Very Large: ${veryLargeJson.length} bytes`);

console.log("\nDone!");
