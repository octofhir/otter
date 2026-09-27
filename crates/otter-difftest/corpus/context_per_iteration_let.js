// Per-iteration lexical bindings (ECMA-262 CreatePerIterationEnvironment).
// Closures made in a `for (let ...)` head's initializer, test and update, and
// in the body, each observe the copy that was current when they were made;
// the copy happens after the body, so body writes carry into the next
// iteration. Covers continue / labeled continue / break / return, const
// heads, for-in / for-of heads with let, const and destructuring, the
// for-of head TDZ, and closures retained across an OSR-hot loop.
function outcome(f) {
  try { return String(f()); } catch (e) { return e.constructor.name; }
}

function headClosures(n) {
  const t = [];
  const u = [];
  let init;
  for (let i = 0, f = () => i; i < n && (t.push(() => i), true); i++, u.push(() => i)) {
    init = f;
  }
  return [init(), t.map(g => g()).join(""), u.map(g => g()).join("")].join("/");
}

function headObjects(n) {
  const t = [];
  const u = [];
  let init;
  for (let box = { n: 0 }, f = () => box.n; box.n < n && (t.push(() => box.n), true);
       box = { n: box.n + 1 }, u.push(() => box.n)) {
    init = f;
  }
  return [init(), t.map(g => g()).join(""), u.map(g => g()).join("")].join("/");
}

function bodyMutation() {
  const fs = [];
  for (let i = 0; i < 6; i++) {
    fs.push(() => i);
    i++;
  }
  const gs = [];
  for (let i = 0; i < 6; i++) {
    gs.push(() => i);
    const bump = () => { i += 2; };
    bump();
  }
  return fs.map(f => f()).join(",") + "|" + gs.map(f => f()).join(",");
}

function controlFlow() {
  const fs = [];
  for (let i = 0; i < 8; i++) {
    if (i % 3 === 1) { fs.push(() => "c" + i); continue; }
    if (i === 6) { fs.push(() => "b" + i); break; }
    fs.push(() => "n" + i);
  }
  const labeled = [];
  outer: for (let i = 0; i < 4; i++) {
    for (let j = 0; j < 4; j++) {
      labeled.push(() => i + ":" + j);
      if (j === i) continue outer;
    }
  }
  const nestedBreak = [];
  for (let i = 0; i < 3; i++) {
    for (let j = 0; j < 5; j++) {
      nestedBreak.push(() => i + ":" + j);
      if (j === 1) break;
    }
  }
  function early() {
    const seen = [];
    for (let k = 0; k < 10; k++) {
      seen.push(() => k);
      if (k === 3) return [seen, () => k * 10];
    }
    return [seen, null];
  }
  const [seen, last] = early();
  return fs.map(f => f()).join(",") + "|" + labeled.map(f => f()).join(",") + "|" +
    nestedBreak.map(f => f()).join(",") + "|" + seen.map(f => f()).join(",") + ":" + last();
}

function constHead() {
  const fs = [];
  for (const box = { n: 0 }; box.n < 3; box.n++) fs.push(() => box.n);
  const hs = [];
  let count = 0;
  for (const k = count; count < 3; count++) hs.push(() => k + count);
  const reassign = outcome(() => { for (const z = 0; z < 1; z++) {} });
  return fs.map(f => f()).join(",") + "|" + hs.map(f => f()).join(",") + "|" + reassign;
}

function forInOf() {
  const src = { a: 1, b: 2, c: 3 };
  const lets = [];
  for (let key in src) { lets.push(() => key); key = key + "!"; }
  const consts = [];
  for (const key in src) consts.push(() => key + src[key]);
  const ofLet = [];
  for (let v of [{ n: 1 }, { n: 2 }, { n: 3 }]) { ofLet.push(() => v.n); v = { n: v.n * 10 }; }
  const ofConst = [];
  for (const v of ["x", "y", "z"]) ofConst.push(() => v);
  const destructured = [];
  for (const [k, { n = 5, m }] of [["p", { m: 1 }], ["q", { n: 2, m: 3 }]]) {
    destructured.push(() => k + n + m);
  }
  const objectHead = [];
  for (let { a, b: [c, d] = [7, 8] } of [{ a: 1 }, { a: 2, b: [3, 4] }]) {
    objectHead.push(() => [a, c, d].join(""));
    a = a * 100;
  }
  const constWrite = outcome(() => { for (const v of [1]) { v = 2; } });
  return [lets, consts, ofLet, ofConst, destructured, objectHead]
    .map(list => list.map(f => f()).join(",")).join("|") + "|" + constWrite;
}

function headTdz() {
  const fs = [];
  const seen = [];
  for (let x of (fs.push(() => typeof x), [{ n: 1 }, { n: 2 }])) seen.push(() => x.n);
  const gs = [];
  for (const y in (gs.push(() => y), { k: 1 })) seen.push(() => y);
  const direct = outcome(() => { for (let z of [z]) {} });
  return outcome(fs[0]) + "," + outcome(gs[0]) + "," + direct + "," + seen.map(f => f()).join("");
}

function osrRetained(n) {
  const kept = [];
  let sum = 0;
  for (let i = 0; i < n; i++) {
    let box = { v: i };
    const read = () => box.v + i;
    sum += read();
    if (i % 500 === 0) kept.push(read);
    if (i % 750 === 0) kept.push(() => (box = { v: -i }, box.v));
  }
  const first = kept.map(f => f()).join(",");
  return sum + "|" + first + "|" + kept.map(f => f()).join(",");
}

function hotHeadClosures(n) {
  let total = 0;
  const heads = [];
  for (let i = 0, f = () => i; i < n; i++, heads.length < 5 && heads.push(() => i)) {
    total += f() + i;
  }
  return total + "|" + heads.map(f => f()).join(",");
}

function hotNestedBreak(n) {
  let sum = 0;
  const kept = [];
  for (let i = 0; i < n; i++) {
    for (let j = 0; j < 5; j++) {
      const f = () => j;
      if (i % 1000 === 3) kept.push(f);
      sum += f();
      if (j === (i & 3)) break;
    }
  }
  return sum + "|" + kept.map(f => f()).join(",");
}

function run() {
  const lines = [];
  lines.push("sloppy=" + (function () { return this !== undefined; })());
  lines.push("head " + headClosures(5));
  lines.push("head-objects " + headObjects(4));
  lines.push("body-mutation " + bodyMutation());
  lines.push("control " + controlFlow());
  lines.push("const-head " + constHead());
  lines.push("for-in-of " + forInOf());
  lines.push("head-tdz " + headTdz());
  lines.push("osr " + osrRetained(3000));
  lines.push("hot-head " + hotHeadClosures(3000));
  lines.push("hot-nested-break " + hotNestedBreak(3000));
  for (const line of lines) console.log(line);
  return "context_per_iteration_let:" + lines.length;
}
run();
