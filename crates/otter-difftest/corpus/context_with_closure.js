// Closures created inside `with (o)` resolve names through `o` at each call:
// adding or deleting `o.x` changes what the closure sees, and assignments go
// to `o` when it has the property, otherwise to the enclosing binding.
// Covers Symbol.unscopables, a function declared in a `with` block, `with`
// inside a per-iteration `let` loop whose variable name is also a property,
// the exact Proxy `has`/`get` trap order, `typeof` of unresolved names,
// eval inside `with`, and a hot loop.
function outcome(f) {
  try { return String(f()); } catch (e) { return e.constructor.name; }
}

function dynamicResolution() {
  var x = { tag: "var" };
  const o = {};
  let read, write;
  with (o) {
    read = () => x.tag;
    write = (v) => { x = { tag: v }; };
  }
  const r = [read()];
  o.x = { tag: "prop" };
  r.push(read());
  write("toProp");
  r.push(o.x.tag, x.tag);
  delete o.x;
  r.push(read());
  write("toVar");
  r.push(x.tag, "x" in o);
  return r.join(",");
}

function unscopables() {
  var hidden = "var-hidden";
  var shown = "var-shown";
  const o = { hidden: "prop-hidden", shown: "prop-shown", [Symbol.unscopables]: { hidden: true } };
  let read;
  with (o) {
    read = () => hidden + "/" + shown;
  }
  const first = read();
  o[Symbol.unscopables].hidden = false;
  const second = read();
  o[Symbol.unscopables] = { shown: true };
  return [first, second, read()].join(",");
}

function functionInWith() {
  const o = { y: { n: 1 } };
  var y = { n: 100 };
  with (o) {
    function inWith() { return y.n; }
    var declared = y.n;
  }
  const first = inWith();
  delete o.y;
  return [first, inWith(), declared, typeof o.inWith].join(",");
}

function loopVariableProperty() {
  const fs = [];
  for (let i = 0; i < 3; i++) {
    const scope = i === 1 ? { i: "prop" + i } : {};
    with (scope) {
      fs.push(() => i);
      fs.push(() => { i = "w" + i; return scope.i; });
    }
  }
  const first = fs.map(f => String(f())).join(",");
  return first;
}

function proxyTrapLog() {
  const log = [];
  const target = { a: 1 };
  const p = new Proxy(target, {
    has(t, k) { log.push("has:" + String(k)); return k in t; },
    get(t, k, r) { log.push("get:" + String(k)); return Reflect.get(t, k, r); },
    set(t, k, v, r) { log.push("set:" + String(k)); return Reflect.set(t, k, v, r); },
  });
  var b = 2;
  let read, write, call;
  with (p) {
    read = () => a + b;
    write = () => { a = 10; b = 20; };
    call = () => typeof missingName;
  }
  const sum = read();
  write();
  const t = call();
  return [sum, target.a, b, t].join(",") + "|" + log.join(",");
}

function unresolved() {
  const o = {};
  let probe;
  with (o) {
    probe = () => [typeof ctxWithUndeclared, outcome(() => ctxWithUndeclared)].join(":");
  }
  const before = probe();
  o.ctxWithUndeclared = { n: 1 };
  return before + "," + probe();
}

function evalInWith() {
  var z = "var-z";
  const o = { z: "prop-z" };
  let r = [];
  with (o) {
    r.push(eval("z"));
    eval("var created = z + '!'");
    eval("z = 'assigned'");
    r.push(eval("(() => z)()"));
    eval("function ew() { return z; }");
  }
  r.push(z, o.z, created, typeof o.created, ew());
  delete o.z;
  r.push(ew());
  return r.join(",");
}

function hot(n) {
  const holders = [{ v: { n: 1 } }, {}, { v: { n: 3 } }];
  var v = { n: 1000 };
  let total = 0;
  const fns = [];
  for (let i = 0; i < 3; i++) {
    with (holders[i]) fns.push(() => v.n);
  }
  for (let i = 0; i < n; i++) {
    total += fns[i % 3]();
    if (i % 1000 === 999) {
      if (holders[1].v) delete holders[1].v;
      else holders[1].v = { n: i };
    }
    if (i % 1500 === 0) v = { n: v.n + 1 };
  }
  return total;
}

function run() {
  const lines = [];
  lines.push("sloppy=" + (function () { return this !== undefined; })());
  lines.push("dynamic " + dynamicResolution());
  lines.push("unscopables " + unscopables());
  lines.push("function " + functionInWith());
  lines.push("loop " + loopVariableProperty());
  lines.push("proxy " + proxyTrapLog());
  lines.push("unresolved " + unresolved());
  lines.push("eval " + evalInWith());
  lines.push("hot " + hot(3000));
  for (const line of lines) console.log(line);
  return "context_with_closure:" + lines.length;
}
run();
