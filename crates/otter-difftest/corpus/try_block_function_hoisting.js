// Try, catch and finally blocks are Blocks: BlockDeclarationInstantiation
// initializes their function declarations at block entry, so a call before
// the declaration statement reaches the function, and the function sees the
// block's `let` bindings (in TDZ until their declarations run). The Annex B
// var binding receives the function when the declaration statement runs.
function outcome(f) {
  try { return String(f()); } catch (e) { return e.constructor.name; }
}

function inTry() {
  try {
    const r = early();
    function early() { return "try-hoisted"; }
    return r;
  } catch (e) {
    return "caught:" + e.constructor.name;
  }
}

function inTryTdz() {
  try {
    const r = outcome(() => f());
    let x = { n: 1 };
    function f() { return x.n; }
    return r + ":" + f();
  } catch (e) {
    return "caught:" + e.constructor.name;
  }
}

function inCatch() {
  try {
    throw { n: 2 };
  } catch (e) {
    const r = outcome(() => c());
    function c() { return "catch" + e.n; }
    return r;
  }
}

function inFinally() {
  let r;
  try {
    r = "try";
  } finally {
    r = outcome(() => fin());
    function fin() { return "finally"; }
  }
  return r;
}

function varBinding() {
  const before = typeof tv;
  let inside;
  try {
    inside = typeof tv;
    function tv() { return "tv"; }
  } finally {
    // nothing
  }
  return [before, inside, typeof tv].join(",");
}

function capturesBlockLet() {
  const fs = [];
  for (let i = 0; i < 3; i++) {
    try {
      fs.push(outcome(() => get()));
      let box = { i };
      function get() { return box.i; }
      fs.push(get());
    } finally {
      fs.push(outcome(() => fget()));
      function fget() { return "f" + i; }
    }
  }
  return fs.join(",");
}

function hot(n) {
  let ok = 0;
  let failed = 0;
  for (let i = 0; i < n; i++) {
    try {
      ok += h(i);
      function h(k) { return k & 1; }
    } catch (e) {
      failed++;
    }
  }
  return ok + ":" + failed;
}

function run() {
  const lines = [];
  lines.push("sloppy=" + (function () { return this !== undefined; })());
  lines.push("try " + inTry());
  lines.push("try-tdz " + inTryTdz());
  lines.push("catch " + inCatch());
  lines.push("finally " + inFinally());
  lines.push("var " + varBinding());
  lines.push("block-let " + capturesBlockLet());
  lines.push("hot " + hot(3000));
  for (const line of lines) console.log(line);
  return "try_block_function_hoisting:" + lines.length;
}
run();
