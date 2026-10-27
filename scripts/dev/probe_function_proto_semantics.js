// Function [[Prototype]] changes and named loads through the function's bag.
const out = [];
function read(f, k) { return f[k]; }
function F() {}
F.own = 1;
for (let i = 0; i < 3000; i++) read(F, 'own');
const p1 = { inherited: 'p1' };
Object.setPrototypeOf(F, p1);
for (let i = 0; i < 3000; i++) read(F, 'inherited');
out.push(read(F, 'own'), read(F, 'inherited'), read(F, 'call'));
const p2 = { inherited: 'p2' };
Object.setPrototypeOf(F, p2);
out.push(read(F, 'inherited'), read(F, 'call'));
Object.setPrototypeOf(F, null);
out.push(read(F, 'inherited'), read(F, 'own'), typeof read(F, 'call'));
class K { static s() { return 'K'; } }
Object.setPrototypeOf(F, K);
out.push(read(F, 'own'), typeof read(F, 's'), read(F, 'name'), read(F, 'length'));
F.extra = 5;
out.push(read(F, 'extra'));
function G() {}
Object.setPrototypeOf(G, p1);
G.late = 7;
out.push(read(G, 'late'), read(G, 'inherited'));
p1.inherited = 'changed';
out.push(read(G, 'inherited'));
const sym = Symbol('s');
G[sym] = 9;
out.push(G[sym], Object.getPrototypeOf(G) === p1);
console.log(JSON.stringify(out));
