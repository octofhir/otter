// Indexed stores that reach past their receiver's elements on hot paths: a
// loop filling a fresh Array by appends from several branch sites, re-entered
// with new arrays (the shape of an emulator's memory-map setup), stores that
// leave gaps, typed-array stores past the end (ignored), and in-bounds
// overwrites at the same sites afterwards.
const results = [];

function fill(table, n, base) {
  for (let i = 0; i < n; i++) {
    if (i < n / 4) {
      table[i] = base + i;
    } else if (i < n / 2) {
      table[i] = base - i;
    } else if (i < (3 * n) / 4) {
      table[i] = i & 1 ? base : i;
    } else {
      table[i] = (i * 7) % 13;
    }
  }
  return table;
}

let sum = 0;
for (let round = 0; round < 40; round++) {
  const table = fill([], 4096 + round * 16, round);
  for (let i = 0; i < table.length; i += 97) sum = (sum + table[i]) % 1000003;
  // Overwrite in place through the same sites.
  fill(table, 1024, -round);
  sum = (sum + table[5] + table[1000] + table.length) % 1000003;
}
results.push(sum);

// Stores with gaps: sparse growth from one site.
function spread(table, n, stride) {
  for (let i = 0; i < n; i++) table[i * stride] = i;
  return table;
}
let gaps = 0;
for (let round = 0; round < 30; round++) {
  const t = spread([], 300, 1 + (round % 3));
  gaps = (gaps + t.length + (t[3] === undefined ? 1 : t[3]) + Object.keys(t).length) % 1000003;
}
results.push(gaps);

// Typed arrays: stores past the end are ignored.
function poke(view, n) {
  for (let i = 0; i < n; i++) view[i] = i * 3;
  return view;
}
let typed = 0;
for (let round = 0; round < 40; round++) {
  const view = poke(new Uint8Array(64 + round), 100);
  typed = (typed + view.length + view[63] + view[view.length - 1]) % 1000003;
}
results.push(typed);

// Holes then fills; elements of objects stay ordinary.
function mixed(t, n) {
  for (let i = n - 1; i >= 0; i -= 2) t[i] = i;
  for (let i = 0; i < n; i += 2) t[i] = -i;
  return t;
}
let holes = 0;
for (let round = 0; round < 30; round++) {
  const t = mixed([], 257);
  const o = mixed({}, 33);
  holes = (holes + t.reduce((a, b) => a + b, 0) + Object.keys(o).length + t.length) % 1000003;
}
results.push(holes);

console.log(JSON.stringify(results));
