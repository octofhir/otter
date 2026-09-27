// Lexical bindings captured by closures survive generator and async
// suspension: closures yielded over function, per-iteration, block and
// finally-block bindings read the right values after later resumes;
// `return()` while suspended inside a block runs `finally` with that
// block's bindings; `throw()` lands in a catch whose parameter is captured;
// async functions and async generators collect closures across `await`.
// Many suspended instances stay live at once; async results print from
// `.then`.
function outcome(f) {
  try { return String(f()); } catch (e) { return e.constructor.name; }
}

function* scopes(seed) {
  let fnLet = { n: seed };
  yield () => fnLet.n;
  for (let i = 0; i < 3; i++) {
    const box = { i };
    yield () => box.i * 10 + i;
  }
  {
    let blockLet = { s: "block" + seed };
    yield () => blockLet.s;
    blockLet = { s: "changed" + seed };
  }
  try {
    fnLet = { n: seed + 1 };
    yield () => "try";
  } finally {
    let finLet = { f: "fin" + seed };
    yield () => finLet.f + ":" + fnLet.n;
  }
}

function generatorClosures() {
  const fns = [...scopes(4)];
  return fns.map(f => f()).join(",");
}

function returnInBlock() {
  const log = [];
  function* g() {
    let outer = { v: "outer" };
    {
      let inner = { v: "inner" };
      const read = () => inner.v + "/" + outer.v;
      try {
        yield read;
        log.push("not-reached");
      } finally {
        log.push("finally:" + read());
        inner = { v: "inner2" };
        log.push("after-write:" + read());
      }
    }
  }
  const it = g();
  const read = it.next().value;
  const ret = it.return("ret");
  log.push("done:" + ret.done + ":" + ret.value, "late:" + read());
  return log.join(",");
}

function throwIntoCatch() {
  function* g() {
    const caught = [];
    for (let round = 0; round < 3; round++) {
      try {
        yield round;
      } catch (err) {
        caught.push(() => err.msg + round);
        err = { msg: "rewritten" };
        caught.push(() => err.msg);
      }
    }
    return caught;
  }
  const it = g();
  it.next();
  it.throw({ msg: "first" });
  it.next();
  const res = it.throw({ msg: "third" });
  return res.done + ":" + res.value.map(f => f()).join(",");
}

function manyInstances(n) {
  const its = [];
  for (let i = 0; i < n; i++) {
    const it = scopes(i);
    it.next();
    its.push(it);
  }
  let total = 0;
  const reads = [];
  for (let step = 0; step < 4; step++) {
    for (let i = 0; i < n; i++) {
      const r = its[i].next();
      const v = r.value();
      if (typeof v === "number") total += v;
      if (i % 500 === 0) reads.push(r.value);
    }
  }
  const finals = its.map(it => { const r = it.next(); return r.value(); });
  return total + ":" + reads.map(f => f()).join("|") + ":" + finals[0] + ":" + finals[n - 1];
}

async function asyncLoop(n) {
  const fns = [];
  let acc = { total: 0 };
  for (let i = 0; i < n; i++) {
    const box = { i };
    await null;
    fns.push(() => box.i + i + acc.total);
    if (i % 2) await Promise.resolve();
    acc = { total: acc.total + 1 };
  }
  return fns.map(f => f()).join(",");
}

async function* asyncGen(limit) {
  for (let i = 0; i < limit; i++) {
    let item = { i };
    await null;
    yield () => item.i * 2;
    item = { i: -i };
  }
}

async function forAwait() {
  const fns = [];
  for await (const f of asyncGen(4)) fns.push(f);
  const direct = [];
  for await (let v of [Promise.resolve(1), 2, Promise.resolve(3)]) direct.push(() => v);
  return fns.map(f => f()).join(",") + "|" + direct.map(f => f()).join(",");
}

async function manyAsync(n) {
  const promises = [];
  for (let i = 0; i < n; i++) {
    promises.push((async () => {
      let local = { i };
      await null;
      const read = () => local.i * 2;
      await null;
      local = { i: local.i + 1 };
      return read();
    })());
  }
  const values = await Promise.all(promises);
  return values.reduce((a, b) => a + b, 0);
}

function run() {
  const lines = [];
  lines.push("sloppy=" + (function () { return this !== undefined; })());
  lines.push("generator " + generatorClosures());
  lines.push("return " + returnInBlock());
  lines.push("throw " + throwIntoCatch());
  lines.push("instances " + manyInstances(1500));
  for (const line of lines) console.log(line);
  asyncLoop(6)
    .then((r) => { console.log("async-loop " + r); return forAwait(); })
    .then((r) => { console.log("for-await " + r); return manyAsync(300); })
    .then((r) => console.log("many-async " + r))
    .catch((e) => console.log("async-error " + e.constructor.name));
  return lines.length;
}
run();
