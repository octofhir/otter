// Object literals opening with `__proto__: null`, hot and cold.
const out = [];
function make(v) { return { __proto__: null, value: v, enumerable: true }; }
function late(v) { return { a: v, __proto__: null }; }
function empty() { return { __proto__: null }; }
let last;
for (let i = 0; i < 20000; i++) { last = [make(i), late(i), empty()]; }
const [m, l, e] = last;
out.push(Object.getPrototypeOf(m) === null, m.value, m.enumerable, 'toString' in m, Object.keys(m).join());
out.push(Object.getPrototypeOf(l) === null, l.a, Object.keys(l).join());
out.push(Object.getPrototypeOf(e) === null, Object.keys(e).length, String(e.x));
e.x = 1; e.y = 2; delete e.x; out.push(Object.keys(e).join(), JSON.stringify(e));
const d = Object.defineProperty({}, 'k', make(5)); out.push(d.k, Object.keys(d).join());
console.log(JSON.stringify(out));
