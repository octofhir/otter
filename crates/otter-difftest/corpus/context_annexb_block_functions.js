// Annex B.3.3 block-level function declarations in sloppy functions: the
// block binding is initialized at block entry, the function-scope var binding
// is `undefined` until the declaration statement runs and then receives the
// block binding's current value. Assignments inside the block hit the block
// binding. Covers `if (c) function h(){}`, switch-case declarations, a
// conflicting `let` suppressing the var binding, block functions capturing
// block and per-iteration `let` bindings, and a hot loop.
function outcome(f) {
  try { return String(f()); } catch (e) { return e.constructor.name; }
}

function typeofAround() {
  const r = [typeof f];
  {
    r.push(typeof f);
    function f() { return "f"; }
  }
  r.push(typeof f, f());
  return r.join(",");
}

function assignmentInBlock() {
  const r = [];
  {
    r.push(typeof g);
    function g() { return "g"; }
    g = { n: 1 };
    r.push(typeof g);
  }
  r.push(typeof g, g());
  {
    early = { n: 2 };
    function early() {}
    early = 3;
  }
  r.push(typeof early, early.n);
  return r.join(",");
}

function ifDeclarations(c) {
  const before = [typeof h, typeof h2];
  if (c) function h() { return "h" + c; }
  if (!c) function h2() { return "h2"; }
  else function h3() { return "h3"; }
  return before.concat([typeof h, typeof h2, typeof h3]).join(",") + ":" +
    (typeof h === "function" ? h() : "-");
}

function switchDeclarations(k) {
  const before = typeof sw1 + typeof sw2;
  switch (k) {
    case 1:
      function sw1() { return "one"; }
      break;
    case 2:
      function sw2() { return "two"; }
  }
  return before + ":" + typeof sw1 + ":" + typeof sw2;
}

function letConflicts() {
  let k = { n: 1 };
  {
    function k() {}
  }
  const outer = typeof k;
  const nested = outcome(() => {
    {
      let q = 1;
      {
        function q() {}
      }
    }
    return typeof q;
  });
  const param = (function (p) {
    {
      function p() {}
    }
    return typeof p;
  })(5);
  const argumentsName = (function () {
    {
      function arguments() {}
    }
    return typeof arguments;
  })();
  return [outer, nested, param, argumentsName].join(",");
}

function capturesBlockLet() {
  {
    let hidden = { v: 1 };
    function readHidden() { return hidden.v; }
    function writeHidden(v) { hidden = { v }; }
    hidden = { v: 2 };
  }
  const first = readHidden();
  writeHidden(9);
  return first + "," + readHidden();
}

function perIteration() {
  const fns = [];
  for (let i = 0; i < 3; i++) {
    let box = { i: i * 10 };
    function perIter() { return i + box.i; }
    fns.push(perIter);
  }
  return fns.map(f => f()).join(",") + ":" + perIter();
}

function hot(n) {
  let total = 0;
  const kept = [];
  for (let i = 0; i < n; i++) {
    let own = { v: i };
    {
      function hotBlock() { return own.v + i; }
    }
    total += hotBlock();
    if (i % 1000 === 0) kept.push(hotBlock);
    if ((i & 1) === 0) {
      function parity() { return own.v & 3; }
    }
    total += parity();
  }
  return total + ":" + kept.map(f => f()).join(",") + ":" + hotBlock() + ":" + parity();
}

function run() {
  const lines = [];
  lines.push("sloppy=" + (function () { return this !== undefined; })());
  lines.push("typeof " + typeofAround());
  lines.push("assign " + assignmentInBlock());
  lines.push("if " + ifDeclarations(1) + "|" + ifDeclarations(0));
  lines.push("switch " + switchDeclarations(1) + "|" + switchDeclarations(2) + "|" + switchDeclarations(3));
  lines.push("let-conflict " + letConflicts());
  lines.push("block-let " + capturesBlockLet());
  lines.push("per-iteration " + perIteration());
  lines.push("hot " + hot(3000));
  for (const line of lines) console.log(line);
  return "context_annexb_block_functions:" + lines.length;
}
run();
