/**
 * JSON benchmark - pure JS, no TypeScript, no Date/performance.now.
 * Compatible with all runtimes: node, bun, deno, otter.
 *
 * Run:
 *   node benchmarks/cpu/json_bench.js
 *   bun benchmarks/cpu/json_bench.js
 *   deno run benchmarks/cpu/json_bench.js
 *   cargo run --release -p otterjs -- benchmarks/cpu/json_bench.js
 */

var now = typeof performance !== "undefined"
    ? function() { return performance.now(); }
    : function() { return Date.now(); };

function bench(name, fn, iterations) {
    // warmup
    fn(); fn(); fn();
    var start = now();
    for (var i = 0; i < iterations; i++) {
        fn();
    }
    var end = now();
    var ms = end - start;
    var ops = (iterations / ms) * 1000;
    console.log(name + ": " + ms.toFixed(1) + "ms (" + Math.round(ops) + " ops/sec)");
    return ms;
}

// ── Test data ───────────────────────────────────────────────────────────

var small = '{"name":"test","value":42,"active":true}';
var smallObj = JSON.parse(small);

// Build medium object manually (no Array.from/Date)
var medium = '{"id":1,"name":"User 1","email":"user1@example.com","active":true,"age":25,"prefs":{"theme":"dark","notify":true,"lang":"en"},"tags":["a","b","c"]}';
var mediumObj = JSON.parse(medium);

// Build large array as string
var parts = [];
for (var i = 0; i < 100; i++) {
    parts.push('{"id":' + i + ',"name":"User ' + i + '","email":"u' + i + '@x.com","active":' + (i % 2 === 0 ? 'true' : 'false') + ',"age":' + (20 + i % 50) + ',"p":{"t":"' + (i % 2 === 0 ? 'dark' : 'light') + '","n":true},"tags":["a","b"]}');
}
var large = '{"users":[' + parts.join(",") + ']}';
var largeObj = JSON.parse(large);

var vparts = [];
for (var j = 0; j < 1000; j++) {
    vparts.push('{"id":' + j + ',"n":"U' + j + '","v":' + (j * 1.1) + '}');
}
var veryLarge = '[' + vparts.join(",") + ']';
var veryLargeObj = JSON.parse(veryLarge);

// ── Benchmarks ──────────────────────────────────────────────────────────

console.log("JSON Benchmark");
console.log("==================================================");

console.log("\nSmall Object (" + small.length + " bytes):");
bench("  parse ", function() { JSON.parse(small); }, 100000);
bench("  stringify", function() { JSON.stringify(smallObj); }, 100000);

console.log("\nMedium Object (" + medium.length + " bytes):");
bench("  parse ", function() { JSON.parse(medium); }, 50000);
bench("  stringify", function() { JSON.stringify(mediumObj); }, 50000);

console.log("\nLarge (100 objects, " + large.length + " bytes):");
bench("  parse ", function() { JSON.parse(large); }, 1000);
bench("  stringify", function() { JSON.stringify(largeObj); }, 1000);

console.log("\nVery Large (1000 objects, " + veryLarge.length + " bytes):");
bench("  parse ", function() { JSON.parse(veryLarge); }, 100);
bench("  stringify", function() { JSON.stringify(veryLargeObj); }, 100);

console.log("\nDone!");
