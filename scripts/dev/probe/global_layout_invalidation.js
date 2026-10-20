// A hot optimized read of a dictionary global object property must see
// every later change: new value, accessor redefinition, deletion,
// re-addition and unrelated global growth.
globalThis.G = 1;
function read() { return G; }
function hot(n) { var s = 0; for (var i = 0; i < n; i++) s += read(); return s; }
var out = [];
out.push(hot(200000));
G = 2;
out.push(hot(1000));
for (var k = 0; k < 300; k++) globalThis["extra" + k] = k;
out.push(hot(1000));
Object.defineProperty(globalThis, "G", { get() { return 5; }, configurable: true });
out.push(hot(1000));
delete globalThis.G;
try { hot(10); out.push("no throw"); } catch (e) { out.push(e.constructor.name); }
globalThis.G = 7;
out.push(hot(1000));
Object.defineProperty(globalThis, "G", { value: 9, writable: false, configurable: true });
out.push(hot(1000));
console.log(out.join(" "));
