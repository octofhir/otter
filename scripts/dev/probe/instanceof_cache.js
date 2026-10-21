// Hot instanceof sites across every change that alters a cached target's answer.
function A() {} function B() {}
const a = new A(), b = new B();
function test(o, F) { return o instanceof F; }
function run(o, F, n) { let c = 0; for (let i = 0; i < n; i++) if (test(o, F)) c++; return c; }
const out = [];
out.push(run(a, A, 20000), run(b, A, 20000), run(a, B, 20000));
// alternate targets at one site
let alt = 0;
for (let i = 0; i < 20000; i++) if (test(i & 1 ? a : b, i & 1 ? A : B)) alt++;
out.push(alt);
// prototype replacement
const oldA = A.prototype;
A.prototype = B.prototype;
out.push(run(a, A, 2000), run(b, A, 2000));
A.prototype = oldA;
out.push(run(a, A, 2000));
// own string property: answer unchanged
A.extra = 1;
out.push(run(a, A, 2000));
// own @@hasInstance on a cached closure (B never had own properties)
out.push(run(a, B, 2000));
Object.defineProperty(B, Symbol.hasInstance, { value: () => true });
out.push(run(a, B, 2000));
// [[Prototype]] override supplying @@hasInstance
function C() {}
out.push(run(a, C, 2000));
Object.setPrototypeOf(C, { [Symbol.hasInstance]() { return true; } });
out.push(run(a, C, 2000));
// prototype becomes a primitive: TypeError
function D() {}
const d = new D();
out.push(run(d, D, 2000));
D.prototype = 3;
let thrown = 0;
for (let i = 0; i < 2000; i++) { try { test(d, D); } catch (e) { if (e instanceof TypeError) thrown++; } }
out.push(thrown);
console.log(JSON.stringify(out));
