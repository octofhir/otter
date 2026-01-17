// SQL Benchmark - Otter
// Tests: Single inserts, SELECT, COPY FROM/TO
import { SQL } from "otter";

const PG_URL = process.env.DATABASE_URL || "postgres://postgres:postgres@localhost:5450/octofhir";
const ITERATIONS = 10000;

const pg = new SQL(PG_URL);

// Setup
await pg.execute`DROP TABLE IF EXISTS bench_sql`;
await pg.execute`CREATE TABLE bench_sql (id SERIAL PRIMARY KEY, name TEXT, value INTEGER, data TEXT)`;

console.log("=== Otter SQL Benchmark ===\n");

// 1. Single INSERT benchmark
console.log(`--- Single INSERT (${ITERATIONS} rows) ---`);
let start = performance.now();
for (let i = 0; i < ITERATIONS; i++) {
    await pg.execute`INSERT INTO bench_sql (name, value, data) VALUES (${"user_" + i}, ${i}, ${"data_" + i.toString().repeat(10)})`;
}
let elapsed = performance.now() - start;
console.log(`Time: ${elapsed.toFixed(2)}ms`);
console.log(`Throughput: ${(ITERATIONS / elapsed * 1000).toFixed(0)} ops/sec\n`);

// 2. SELECT benchmark
console.log(`--- SELECT ${ITERATIONS} rows ---`);
start = performance.now();
const rows = await pg.query`SELECT * FROM bench_sql`;
elapsed = performance.now() - start;
console.log(`Time: ${elapsed.toFixed(2)}ms`);
console.log(`Rows: ${rows.length}\n`);

// 3. COPY FROM benchmark (Otter exclusive!)
await pg.execute`TRUNCATE bench_sql`;

console.log(`--- COPY FROM (${ITERATIONS} rows) ---`);
let csvData = "";
for (let i = 0; i < ITERATIONS; i++) {
    csvData += `user_copy_${i},${i},data_${i.toString().repeat(10)}\n`;
}

start = performance.now();
const copied = await pg.copyFrom("bench_sql", {
    columns: ["name", "value", "data"],
    format: "csv",
    source: new Blob([csvData]),
});
elapsed = performance.now() - start;
console.log(`Time: ${elapsed.toFixed(2)}ms`);
console.log(`Rows: ${copied}`);
console.log(`Throughput: ${(copied / elapsed * 1000).toFixed(0)} rows/sec\n`);

// 4. COPY TO benchmark
console.log(`--- COPY TO (${ITERATIONS} rows) ---`);
start = performance.now();
let exportSize = 0;
for await (const chunk of await pg.copyTo("bench_sql", { format: "csv" })) {
    exportSize += chunk.length;
}
elapsed = performance.now() - start;
console.log(`Time: ${elapsed.toFixed(2)}ms`);
console.log(`Size: ${(exportSize / 1024).toFixed(1)} KB\n`);

// Cleanup
await pg.execute`DROP TABLE bench_sql`;
await pg.close();

console.log("=== Benchmark Complete ===");
