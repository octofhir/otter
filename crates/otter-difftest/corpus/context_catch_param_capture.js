// Catch parameters are per-catch lexical bindings that closures capture like
// any other: writes after capture are visible, the parameter shadows an
// outer `var` of the same name, Annex B.3.5 `var e = 3` inside the catch
// assigns the parameter while hoisting an outer var, destructuring
// parameters with defaults are captured, every throw in a hot loop gets a
// fresh binding, nested catches keep both parameters, eval inside the catch
// reads and writes the parameter, and a catch binding survives a `yield`.
function outcome(f) {
  try { return String(f()); } catch (e) { return e.constructor.name; }
}

function writeAfterCapture() {
  const fs = [];
  try {
    throw { tag: "thrown" };
  } catch (e) {
    fs.push(() => e);
    e = 2;
    fs.push(() => e);
  }
  return fs.map(f => String(f())).join(",") + "," + typeof e;
}

function shadowsOuterVar() {
  var e = { tag: "outer" };
  const readOuter = () => e.tag;
  let inner;
  try {
    throw { tag: "inner" };
  } catch (e) {
    inner = () => e.tag;
    e = { tag: "inner2" };
  }
  return [readOuter(), inner(), e.tag].join(",");
}

function annexBVar() {
  const before = typeof e;
  let inner;
  try {
    throw { tag: "thrown" };
  } catch (e) {
    var e = 3;
    inner = () => e;
  }
  const forIn = (function () {
    try { throw 1; } catch (x) { for (var x in { key: 1 }) {} return x; }
  })();
  const forOfError = outcome(() => eval("try { throw 1; } catch (y) { for (var y of [2]) {} }"));
  return [before, inner(), String(e), forIn, forOfError].join(",");
}

function destructuring() {
  const fs = [];
  try {
    throw { a: { n: 1 } };
  } catch ({ a, b = { n: 2 }, c: [d = { n: 3 }] = [] }) {
    fs.push(() => a.n + b.n + d.n);
    b = { n: 20 };
    fs.push(() => b.n);
  }
  try {
    throw [undefined, { n: 5 }];
  } catch ([first = { n: 4 }, second]) {
    fs.push(() => first.n * second.n);
  }
  const direct = (function () {
    try { throw { q: { n: 6 } }; } catch ({ q }) { q = { n: q.n + 1 }; return q.n; }
  })();
  const bad = outcome(() => { try { throw null; } catch ({ x }) { return x; } });
  return fs.map(f => outcome(f)).join(",") + "," + direct + "," + bad;
}

function hotThrows(n) {
  const kept = [];
  let sum = 0;
  for (let i = 0; i < n; i++) {
    try {
      if (i >= 0) throw { i };
    } catch (err) {
      const read = () => err.i;
      sum += read();
      if (i % 1000 === 0) kept.push(read);
      if (i % 1500 === 0) kept.push(() => (err = { i: -1 }, err.i));
    }
  }
  const once = kept.map(f => f()).join(",");
  return sum + "|" + once + "|" + kept.map(f => f()).join(",");
}

function nestedCatches() {
  let outerRead;
  let innerRead;
  let both;
  try {
    throw { n: 1 };
  } catch (outer) {
    try {
      throw { n: 2 };
    } catch (inner) {
      outerRead = () => outer.n;
      innerRead = () => inner.n;
      both = () => { outer = { n: outer.n + 10 }; return outer.n + inner.n; };
    }
    outer = { n: 100 };
  }
  const shadow = (function () {
    try { throw "a"; } catch (e) {
      try { throw "b"; } catch (e) { return (() => e)(); }
    }
  })();
  return [outerRead(), innerRead(), both(), outerRead(), shadow].join(",");
}

function evalInCatch() {
  const results = [];
  try {
    throw { n: 1 };
  } catch (e) {
    const read = () => (typeof e === "object" ? e.n : e);
    eval("e = 5");
    results.push(read());
    eval("var e = 6");
    results.push(read(), eval("e"));
    results.push(eval("typeof e"));
  }
  results.push(String(e));
  const pattern = outcome(() => { try { throw [1]; } catch ([p]) { eval("p = 7"); return p; } });
  results.push(pattern);
  return results.join(",");
}

function acrossYield() {
  function* g() {
    const fs = [];
    for (let round = 0; round < 2; round++) {
      try {
        throw { round };
      } catch (err) {
        const read = () => err.round;
        const got = yield read;
        err = { round: got };
        fs.push(read);
      }
    }
    return fs;
  }
  const it = g();
  const r0 = it.next().value;
  const before = r0();
  const r1 = it.next(10).value;
  const done = it.next(20);
  return [before, r0(), r1(), done.value.map(f => f()).join("/")].join(",");
}

function run() {
  const lines = [];
  lines.push("sloppy=" + (function () { return this !== undefined; })());
  lines.push("write " + writeAfterCapture());
  lines.push("shadow " + shadowsOuterVar());
  lines.push("annexb " + annexBVar());
  lines.push("destructuring " + destructuring());
  lines.push("hot " + hotThrows(3000));
  lines.push("nested " + nestedCatches());
  lines.push("eval " + evalInCatch());
  lines.push("yield " + acrossYield());
  for (const line of lines) console.log(line);
  return "context_catch_param_capture:" + lines.length;
}
run();
