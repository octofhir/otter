// Re-parented functions: inherited statics, accessors, synthesized keys,
// enumeration and later changes along the chain.
const out = [];
function read(f, k) { return f[k]; }
function A() {}
A.shared = 'A';
Object.defineProperty(A, 'acc', { get() { return 'acc:' + this.name; }, configurable: true });
function B() {}
B.own = 'B';
Object.setPrototypeOf(B, A);
function C() {}
C.c = 1;
Object.setPrototypeOf(C, B);
for (let i = 0; i < 3000; i++) { read(C, 'shared'); read(C, 'acc'); read(C, 'call'); read(C, 'missing'); }
out.push(read(C, 'shared'), read(C, 'acc'), typeof read(C, 'call'), read(C, 'missing'), read(C, 'own'));
delete C.name;
out.push(read(C, 'name'), read(C, 'length'));
const keys = []; for (const k in C) keys.push(k); out.push(keys.join(','));
A.shared = 'A2'; out.push(read(C, 'shared'));
B.shared = 'B-shadow'; out.push(read(C, 'shared'));
delete B.shared; out.push(read(C, 'shared'));
A.missing = 'now'; out.push(read(C, 'missing'));
Object.setPrototypeOf(B, { shared: 'obj' }); out.push(read(C, 'shared'), typeof read(C, 'call'));
class K { static ks() { return 'K'; } }
function D() {}
D.d = 1;
Object.setPrototypeOf(D, K);
for (let i = 0; i < 3000; i++) read(D, 'ks');
out.push(read(D, 'ks')(), read(D, 'name'));
class E extends B {}
out.push(read(E, 'own'), read(E, 'shared'));
console.log(JSON.stringify(out));
