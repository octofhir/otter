function f(a, i) { return a[i] | 0; }
function g(a, i) { const v = a[i]; return v + 1; }
const arr = [1.5, -0, 3, 2 ** 31, -(2 ** 31), NaN, 7, 4294967296, -1];
let s = 0;
const ints = new Array(64).fill(0).map((_, i) => i * 1.0);
for (let r = 0; r < 20000; r++) for (let i = 0; i < 64; i++) s = (s + g(ints, i)) | 0;
for (let r = 0; r < 2000; r++) for (let i = 0; i < arr.length; i++) s = (s + f(arr, i) + (Object.is(g(arr, i) - 1, -0) ? 1 : 0)) | 0;
console.log(s);
