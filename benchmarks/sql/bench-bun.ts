// SQL Benchmark - Bun
// Tests: Single inserts, SELECT (no COPY support in Bun!)
import { SQL } from "bun";

const PG_URL = process.env.DATABASE_URL || "postgres://postgres:postgres@localhost:5450/octofhir";
const ITERATIONS = 10000;

const pg = new SQL(PG_URL);

// Setup
await pg`DROP TABLE IF EXISTS bench_sql`;
await pg`CREATE TABLE bench_sql (id SERIAL PRIMARY KEY, name TEXT, value INTEGER, data TEXT)`;

console.log("=== Bun SQL Benchmark ===\n");

// 1. Single INSERT benchmark
console.log(`--- Single INSERT (${ITERATIONS} rows) ---`);
let start = performance.now();
for (let i = 0; i < ITERATIONS; i++) {
    await pg`INSERT INTO bench_sql (name, value, data) VALUES (${"user_" + i}, ${i}, ${"data_" + i.toString().repeat(10)})`;
}
let elapsed = performance.now() - start;
console.log(`Time: ${elapsed.toFixed(2)}ms`);
console.log(`Throughput: ${(ITERATIONS / elapsed * 1000).toFixed(0)} ops/sec\n`);

// 2. SELECT benchmark
console.log(`--- SELECT ${ITERATIONS} rows ---`);
start = performance.now();
const rows = await pg`SELECT * FROM bench_sql`;
elapsed = performance.now() - start;
console.log(`Time: ${elapsed.toFixed(2)}ms`);
console.log(`Rows: ${rows.length}\n`);

// 3. COPY FROM - NOT SUPPORTED IN BUN!
console.log(`--- COPY FROM ---`);
console.log(`NOT SUPPORTED in Bun (see https://github.com/oven-sh/bun/pull/23350)\n`);

// 4. COPY TO - NOT SUPPORTED IN BUN!
console.log(`--- COPY TO ---`);
console.log(`NOT SUPPORTED in Bun\n`);

// Cleanup
await pg`DROP TABLE bench_sql`;
await pg.close();

console.log("=== Benchmark Complete ===");
