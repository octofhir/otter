// Integer keys on a plain object used as a map: `in`, reads, writes.
"use strict";
const map = {};
for (let i = 0; i < 100; i++) map[i * 3] = i;
let hits = 0, sum = 0;
function probe() {
  for (let i = 0; i < 1000; i++) {
    if ((i % 300) in map) hits++;
    sum += map[(i % 100) * 3];
    map[(i % 50) * 3] = i;
  }
}
for (let i = 0; i < 50; i++) probe();
const t = Date.now();
for (let i = 0; i < 3000; i++) probe();
const ms = Date.now() - t;
console.log((ms * 1e6 / 3e6).toFixed(1), "ns/iter", hits, sum);
