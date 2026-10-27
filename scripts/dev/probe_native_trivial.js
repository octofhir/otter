// Bare native call boundary: a builtin whose work is a few loads.
// usage: otter probe_native_trivial.js <iterations>
const n = Number(process.argv[2] || 10_000_000);
const o = {};
let s = 0;
for (let i = 0; i < n; i++) s += Object.isExtensible(o) ? 1 : 0;
console.log(s);
