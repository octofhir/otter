// Closures observe the temporal dead zone of the lexical bindings they
// capture. A read or write before initialization throws ReferenceError and
// leaves the binding uninitialized; `typeof` does not shield a captured TDZ
// binding; a switch-case binding whose declaration never runs stays in TDZ;
// class heritage sees the inner class name in TDZ; parameter defaults see
// later parameters in TDZ. Captured bindings hold fresh objects so GC stress
// moves them, and the block case runs hot enough to tier up.
function outcome(f) {
  try {
    const value = f();
    return "ok:" + (value && typeof value === "object" ? JSON.stringify(value) : String(value));
  } catch (e) {
    return e.constructor.name;
  }
}

function functionLevel() {
  const read = () => value.tag;
  const before = outcome(read);
  let value = { tag: "init" };
  const after = outcome(read);
  value = { tag: "second" };
  return [before, after, outcome(read)].join(",");
}

function hotBlocks(n) {
  let caught = 0;
  let sum = 0;
  const keep = [];
  for (let i = 0; i < n; i++) {
    {
      const read = () => cell.n;
      if (i === 0) {
        try { read(); } catch (e) { if (e instanceof ReferenceError) caught++; }
      }
      let cell = { n: i };
      sum += read();
      if (i % 1000 === 0) keep.push(read);
    }
  }
  return caught + ":" + sum + ":" + keep.map(f => f()).join("|");
}

function typeofThroughClosure() {
  const probe = () => typeof hidden;
  const before = outcome(probe);
  let hidden = { kind: "object" };
  return before + "," + outcome(probe) + "," + outcome(() => typeof neverDeclaredAnywhere);
}

function switchCase(k) {
  let f;
  switch (k) {
    case 0:
      f = () => later;
      break;
    case 1:
      let later = { v: 1 };
      f = () => later;
      break;
  }
  return outcome(f);
}

function switchFallthrough() {
  let f;
  switch (2) {
    case 2:
      f = () => later2.v;
    case 3:
      let later2 = { v: 9 };
  }
  return outcome(f);
}

function hotSwitch(n) {
  let refs = 0;
  let oks = 0;
  for (let i = 0; i < n; i++) {
    const r = switchCase(i % 2);
    if (r === "ReferenceError") refs++;
    else if (r.startsWith("ok:")) oks++;
  }
  return refs + "/" + oks;
}

function writeBeforeInit() {
  const write = () => { slot = { n: 5 }; };
  const read = () => slot.n;
  const w1 = outcome(write);
  const r1 = outcome(read);
  let slot = { n: 1 };
  const r2 = outcome(read);
  const w2 = outcome(write);
  return [w1, r1, r2, w2, read()].join(",");
}

function constAssignment() {
  const early = () => { c2 = { n: 3 }; };
  const e1 = outcome(early);
  const k = { n: 1 };
  const write = () => { k = { n: 2 }; };
  const compound = () => { k.n += 1; k += 1; };
  const c2 = { n: 4 };
  const e2 = outcome(early);
  return [e1, e2, outcome(write), outcome(compound), k.n, c2.n].join(",");
}

function classHeritage() {
  let g;
  let during;
  class C extends (g = () => C, during = outcome(() => g().name), Object) {
    tag() { return "inner"; }
  }
  const original = C;
  C = null;
  return [during, g() === original, C === null, new (g())().tag()].join(",");
}

function p(a = () => b, b = { v: 1 }) { return a().v; }
function q(a = b, b) { return a; }
function q2(a = (() => b)(), b) { return a; }
function paramScopes(n) {
  let sum = 0;
  let thrown = 0;
  for (let i = 0; i < n; i++) {
    try { sum += p(undefined, { v: i & 7 }); } catch (e) { thrown++; }
  }
  return [sum, thrown, outcome(() => p()), outcome(() => q()), outcome(() => q(7)), outcome(() => q2()),
    outcome(() => q2(undefined, 3)), outcome(() => q2(8))].join(",");
}

function run() {
  const lines = [];
  lines.push("sloppy=" + (function () { return this !== undefined; })());
  lines.push("function-level " + functionLevel());
  lines.push("hot-blocks " + hotBlocks(3000));
  lines.push("typeof " + typeofThroughClosure());
  lines.push("switch " + [switchCase(0), switchCase(1), switchFallthrough()].join(","));
  lines.push("hot-switch " + hotSwitch(3000));
  lines.push("write-before-init " + writeBeforeInit());
  lines.push("const " + constAssignment());
  lines.push("heritage " + classHeritage());
  lines.push("params " + paramScopes(3000));
  for (const line of lines) console.log(line);
  return "context_tdz_closures:" + lines.length;
}
run();
