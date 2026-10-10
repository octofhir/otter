// Hot ToPrimitive over ordinary objects: relational compares, string and
// numeric concatenation, template literals and String(). Each conversion
// must observe valueOf / toString / @@toPrimitive / @@toStringTag added or
// replaced mid-loop on the instance, its class prototype or
// %Object.prototype%, and fall back for chains that skip %Object.prototype%.
function Node(id) { this.id = id; }
class Tagged { constructor(id) { this.id = id; } }

function compareLoop(objects, n, hook) {
  let less = 0, text = 0, plus = "";
  for (let i = 0; i < n; i++) {
    if (hook) hook(i);
    const a = objects[i % objects.length];
    const b = objects[(i + 1) % objects.length];
    if (a < b) less++;
    if (a >= b) less += 2;
    text += String(a).length + `${b}`.length;
    if (i % 1000 === 0) plus += a + 1;
  }
  return less + ":" + text + ":" + plus;
}

const out = [];
const nodes = [new Node(1), new Node(2), new Node(3)];
out.push(compareLoop(nodes, 6000, null));

// valueOf appears on the class prototype, then on one instance.
out.push(compareLoop(nodes, 6000, (i) => {
  if (i === 2000) Node.prototype.valueOf = function () { return this.id; };
  if (i === 4000) nodes[1].valueOf = function () { return 100; };
}));
delete Node.prototype.valueOf;
delete nodes[1].valueOf;

// @@toStringTag on the prototype as data, then as a getter.
out.push(compareLoop([new Tagged(1), new Tagged(2)], 6000, (i) => {
  if (i === 1500) Tagged.prototype[Symbol.toStringTag] = "T";
  if (i === 3000) {
    Object.defineProperty(Tagged.prototype, Symbol.toStringTag, {
      configurable: true,
      get() { return "G" + this.id; },
    });
  }
  if (i === 4500) delete Tagged.prototype[Symbol.toStringTag];
}));

// @@toPrimitive on an instance mid-loop.
const prim = [new Node(5), new Node(6)];
out.push(compareLoop(prim, 4000, (i) => {
  if (i === 2000) prim[0][Symbol.toPrimitive] = (hint) => hint === "number" ? 9 : "p" + hint;
}));

// %Object.prototype% itself changes and is restored.
const savedToString = Object.prototype.toString;
const savedValueOf = Object.prototype.valueOf;
out.push(compareLoop([new Node(7), new Node(8)], 6000, (i) => {
  if (i === 1000) Object.prototype.valueOf = function () { return this.id * 10; };
  if (i === 2000) Object.prototype.valueOf = savedValueOf;
  if (i === 3000) Object.prototype.toString = function () { return "x" + this.id; };
  if (i === 4000) Object.prototype.toString = savedToString;
  if (i === 5000) Object.prototype[Symbol.toStringTag] = "Root";
}));
delete Object.prototype[Symbol.toStringTag];

// Arguments objects, chains without %Object.prototype%, and proxies.
function args() { return arguments; }
let mixed = "";
for (let i = 0; i < 3000; i++) {
  const a = args(i);
  if (i % 500 === 0) mixed += String(a) + (a < a);
}
const bare = Object.create(null);
let thrown = 0;
for (let i = 0; i < 3000; i++) {
  try { if (bare < 1) mixed += "?"; } catch (e) { thrown++; }
}
const proxied = new Proxy({}, { get(t, k) { return k === "toString" ? () => "proxied" : undefined; } });
for (let i = 0; i < 3000; i++) if (i % 1000 === 0) mixed += String(proxied) + (proxied < "q");
out.push(mixed, thrown);
console.log(out.join(","));
