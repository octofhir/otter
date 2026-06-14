// Array.prototype.sort with comparator — comparator call overhead + sort.
function lcg(seed) {
  let s = seed >>> 0;
  return () => (s = (s * 1664525 + 1013904223) >>> 0) / 4294967296;
}
let acc = 0;
for (let r = 0; r < 40; r++) {
  const rng = lcg(r + 1);
  const arr = new Array(20_000);
  for (let i = 0; i < arr.length; i++) arr[i] = (rng() * 1e9) | 0;
  arr.sort((a, b) => a - b);
  acc += arr[0] + arr[arr.length - 1] + arr[arr.length >> 1];
}
console.log(acc);
