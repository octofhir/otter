/**
 * JSON benchmark — compatible with otter new VM, node, bun, deno.
 * Uses Date.now() for timing (available everywhere).
 *
 * Run:
 *   node benchmarks/cpu/json_bench_simple.js
 *   bun benchmarks/cpu/json_bench_simple.js
 *   ./target/release/otter-newvm benchmarks/cpu/json_bench_simple.js
 */

function bench(name, fn, iterations) {
    fn(); fn(); fn();
    var start = Date.now();
    var i = 0;
    while (i < iterations) {
        fn();
        i = i + 1;
    }
    var elapsed = Date.now() - start;
    var ops = 0;
    if (elapsed > 0) {
        ops = ((iterations / elapsed) * 1000) | 0;
    }
    console.log(name + ": " + elapsed + "ms (" + ops + " ops/sec, " + iterations + " iters)");
}

// ── Test data ─────────────────────────────────────────────────────────

var small = '{"name":"test","value":42,"active":true}';
var smallObj = JSON.parse(small);

var medium = '{"id":1,"name":"User 1","email":"user1@example.com","active":true,"age":25,"prefs":{"theme":"dark","notify":true,"lang":"en"},"tags":["a","b","c"]}';
var mediumObj = JSON.parse(medium);

var parts = [];
var i = 0;
while (i < 100) {
    var active = (i % 2 === 0) ? "true" : "false";
    var theme = (i % 2 === 0) ? "dark" : "light";
    var age = 20 + (i % 50);
    parts.push('{"id":' + i + ',"name":"User ' + i + '","email":"u' + i + '@x.com","active":' + active + ',"age":' + age + ',"p":{"t":"' + theme + '","n":true},"tags":["a","b"]}');
    i = i + 1;
}
var large = '{"users":[' + parts.join(",") + ']}';
var largeObj = JSON.parse(large);

// ── Benchmarks ────────────────────────────────────────────────────────

console.log("JSON Benchmark");
console.log("==================================================");

console.log("");
console.log("Small Object (" + small.length + " bytes):");
bench("  parse    ", function() { JSON.parse(small); }, 50000);
bench("  stringify", function() { JSON.stringify(smallObj); }, 50000);

console.log("");
console.log("Medium Object (" + medium.length + " bytes):");
bench("  parse    ", function() { JSON.parse(medium); }, 20000);
bench("  stringify", function() { JSON.stringify(mediumObj); }, 20000);

console.log("");
console.log("Large (100 objects, " + large.length + " bytes):");
bench("  parse    ", function() { JSON.parse(large); }, 500);
bench("  stringify", function() { JSON.stringify(largeObj); }, 500);

console.log("");
console.log("Done!");
