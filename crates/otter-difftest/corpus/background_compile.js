// Optimized code compiled while the program keeps running: many functions
// cross their tier thresholds together, feedback changes while their
// compilations are in flight, allocation churn collects while snapshots wait,
// long loops are entered through OSR, and a speculation fails right after
// its body is installed.
const results = [];

// Sixteen distinct hot functions requested back to back.
const makers = [];
for (let k = 0; k < 16; k++) {
  makers.push(new Function('a', 'b', `
    let s = ${k};
    for (let i = 0; i < a.length; i++) s = (s * 31 + a[i] * ${k + 1} + b) | 0;
    return s;
  `));
}
const data = [];
for (let i = 0; i < 64; i++) data.push((i * 7919) % 1013);
let mixed = 0;
for (let round = 0; round < 400; round++) {
  for (let k = 0; k < makers.length; k++) mixed = (mixed + makers[k](data, round)) | 0;
}
results.push(mixed);

// Feedback widens while the first compilation of `shape` may be in flight:
// new receiver layouts, then doubles, then strings.
function Point(x, y) { this.x = x; this.y = y; }
function Point3(x, y, z) { this.x = x; this.y = y; this.z = z; }
function shape(p) { return p.x * 2 + p.y; }
let widened = 0;
for (let i = 0; i < 30000; i++) {
  let p;
  if (i < 20000) p = new Point(i, 1);
  else if (i < 25000) p = new Point3(i, 0.5, 2);
  else if (i < 28000) p = { y: i, x: 3 };
  else p = { x: '7', y: i };
  widened = (widened + (shape(p) | 0)) % 1000003;
}
results.push(widened);

// Garbage while compilations wait: the hidden classes their snapshots name
// must survive every collection until the code is installed.
function Node(value, next) { this.value = value; this.next = next; }
function sumList(list) { let s = 0; for (let n = list; n; n = n.next) s += n.value; return s; }
let lists = 0;
for (let round = 0; round < 300; round++) {
  let list = null;
  for (let i = 0; i < 200; i++) list = new Node(i ^ round, list);
  const junk = [];
  for (let i = 0; i < 400; i++) junk.push({ round, i, text: 'x' + i });
  lists = (lists + sumList(list) + junk.length) % 1000003;
}
results.push(lists);

// One long-running loop that can only reach optimized code through OSR,
// with a second loop after it in the same activation.
function longLoops(n) {
  let a = 0;
  for (let i = 0; i < n; i++) a = (a + i * i) % 65521;
  let b = 1;
  for (let j = 0; j < n; j++) b = (b * 3 + j) % 65521;
  return a * 65536 + b;
}
results.push(longLoops(3000000));

// A speculation fails after the body is installed: int32 arithmetic that
// overflows, then a receiver without the field.
function accumulate(o, n) {
  let s = 0;
  for (let i = 0; i < n; i++) s += o.k * i;
  return s;
}
const ints = { k: 3 };
let accumulated = 0;
for (let i = 0; i < 2000; i++) accumulated = (accumulated + accumulate(ints, 100)) % 1000003;
results.push(accumulated, accumulate({ k: 2147483647 }, 4), String(accumulate({}, 3)),
  accumulate({ k: 1.5 }, 4));

console.log(JSON.stringify(results));
