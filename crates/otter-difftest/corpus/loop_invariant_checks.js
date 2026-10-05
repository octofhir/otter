// Checks and loads that run once before a loop: typed arrays and closure
// variables read in hot loops, a closure variable that later names another
// kind of array, a loop that writes the closure variable it reads, plain
// arrays that turn holey or double, a loop that adds properties to the
// object it reads, and bitwise operators on comparison results.
let heap = new Int32Array(256);
for (let i = 0; i < heap.length; i++) heap[i] = i * 3;
function sumHeap(n) { let s = 0; for (let i = 0; i < n; i++) s = (s + heap[i & 255]) | 0; return s; }
let total = 0;
for (let r = 0; r < 200; r++) total = (total + sumHeap(1000)) | 0;
heap = new Float64Array(256).fill(1.5);
total += sumHeap(1000);
let cur = new Int32Array(64).fill(1), other = new Int32Array(64).fill(2);
function swapping(n) {
  let s = 0;
  for (let i = 0; i < n; i++) {
    s += cur[i & 63];
    if ((i & 7) === 7) { const t = cur; cur = other; other = t; }
  }
  return s;
}
for (let r = 0; r < 200; r++) total += swapping(500);
const plain = [];
for (let i = 0; i < 100; i++) plain.push(i);
function sumPlain(a, n) { let s = 0; for (let i = 0; i < n; i++) s += a[i % a.length]; return s; }
for (let r = 0; r < 300; r++) total += sumPlain(plain, 400);
const holey = [1, 2, , 4];
total += sumPlain(holey.map((x) => x), 100);
total += sumPlain([1.5, 2.5, 3.5], 100);
function shapes(o, n) {
  let s = 0;
  for (let i = 0; i < n; i++) { s += o.x; if (i === n - 2) o['y' + i] = i; }
  return s;
}
for (let r = 0; r < 300; r++) total += shapes({ x: r }, 50);
function flags(a, b, n) {
  let s = 0;
  for (let i = 0; i < n; i++) s = (s + ((a[i & 15] & (i < b)) | ((i > 3) ^ (a[i & 7] > 2)))) | 0;
  return s;
}
const small = new Uint8Array(16).map((_, i) => i * 7);
for (let r = 0; r < 300; r++) total = (total + flags(small, r & 31, 64)) | 0;
console.log(total);
