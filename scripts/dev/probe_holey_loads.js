// Reads of a holey array built by scattered stores, as a token table is.
const table = [];
for (let i = 0; i < 150; i += 3) table[i] = "t" + i;
function read(t) { return table[t]; }
let hits = 0;
const t0 = Date.now();
for (let n = 0; n < 3000000; n++) if (read(n % 160) !== undefined) hits++;
console.log(hits, Date.now() - t0, "ms");
