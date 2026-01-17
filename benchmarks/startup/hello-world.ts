/**
 * Minimal startup benchmark - measures time to execute the simplest possible script.
 *
 * Run with:
 *   time otter run benchmarks/startup/hello-world.ts
 *   time node --experimental-strip-types benchmarks/startup/hello-world.ts
 *   time bun benchmarks/startup/hello-world.ts
 *
 * Or use the benchmark runner:
 *   ./benchmarks/bench.sh startup/hello-world.ts
 */

console.log("Hello, World!");
