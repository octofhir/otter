// Bare native call boundary from optimized code: builtins whose work is a few
// instructions and that have no dedicated leaf entry.
// usage: otter probe_native_trivial.js <iterations> [cbrt|isExtensible]
const n = Number(process.argv[2] || 10_000_000);
const which = process.argv[3] || 'cbrt';
const o = {};
function cbrt(n) { let s = 0; for (let i = 0; i < n; i++) s += Math.cbrt(8); return s; }
function isExtensible(n) { let s = 0; for (let i = 0; i < n; i++) s += Object.isExtensible(o) ? 1 : 0; return s; }
console.log(which === 'cbrt' ? cbrt(n) : isExtensible(n));
