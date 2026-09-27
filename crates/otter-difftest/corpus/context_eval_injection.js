// Sloppy direct eval injects `var` and function declarations into the
// caller's variable environment. Closures created before the eval must see
// the injected binding shadow an outer captured one, and `delete` of the
// eval var restores outer resolution. Also covers arrows owning their own
// variable environment, nested eval, var/let conflicts, strict isolation,
// indirect eval, eval reads of deep block and catch bindings, const and
// named-function-expression self-binding assignment, and hot sibling
// closures of a function that contains eval.
function outcome(f) {
  try {
    const value = f();
    return value && typeof value === "object" ? JSON.stringify(value) : String(value);
  } catch (e) {
    return e.constructor.name;
  }
}

function shadowAndDelete() {
  var x = { v: "outer" };
  function inner(src) {
    const read = () => x === undefined ? "undef" : x.v;
    const before = read();
    eval(src);
    const shadowed = read();
    const deleted = delete x;
    const restored = read();
    return [before, shadowed, deleted, restored].join(",");
  }
  const plain = inner("var x = { v: 'eval' }");
  const noInit = inner("var x");
  const outerWrite = (function () {
    const write = () => { x = { v: "written" }; };
    eval("var x = { v: 'local' }");
    write();
    const local = x.v;
    delete x;
    return local + "/" + x.v;
  })();
  return plain + "|" + noInit + "|" + outerWrite;
}

function evalFunction() {
  const call = () => typeof ef === "function" ? ef(2) : typeof ef;
  const before = call();
  eval("function ef(n) { return { twice: n * 2 }.twice; }");
  const after = call();
  const redefined = (eval("function ef(n) { return n + 100; }"), call());
  return [before, after, redefined, delete ef, call()].join(",");
}

function arrowOwnsVarEnv() {
  const arrow = () => { eval("var av = { n: 1 }"); return typeof av; };
  const inArrow = arrow();
  const nested = () => () => { eval("var av2 = 2"); return av2; };
  return [inArrow, typeof av, nested()(), typeof av2].join(",");
}

function nestedEval() {
  const read = () => typeof deep === "undefined" ? "none" : deep.n;
  const before = read();
  eval("eval('var deep = { n: 3 }')");
  return before + "," + read() + "," + delete deep + "," + read();
}

function conflicts() {
  const blockLet = outcome(() => { { let b; eval("var b"); } });
  const blockLetOk = outcome(() => { { let b2 = 1; eval("var c2 = b2 + 1"); } return c2; });
  const outerLet = outcome(() => { let top = 1; { eval("var top = 2"); } return top; });
  const catchParam = outcome(() => { try { throw 1; } catch (e) { eval("var e = 7"); return e; } });
  const catchPattern = outcome(() => { try { throw [1]; } catch ([e]) { eval("var e = 8"); } });
  const funcInBlock = outcome(() => { let fb = 1; { eval("function fb() {}"); } });
  return [blockLet, blockLetOk, outerLet, catchParam, catchPattern, funcInBlock].join(",");
}

function strictIsolation() {
  const strictCaller = (function () {
    "use strict";
    eval("var sv = { n: 1 }");
    return typeof sv;
  })();
  const strictSource = (function () {
    eval("'use strict'; var sv2 = 2");
    return typeof sv2;
  })();
  const strictFunction = (function () {
    eval("function sf() {}");
    return (function () { "use strict"; eval("function sf2() {}"); return typeof sf2; })() + "/" + typeof sf;
  })();
  return [strictCaller, strictSource, strictFunction].join(",");
}

function indirectGlobal() {
  var ctxEvalProbe = "local";
  (0, eval)("var ctxEvalProbe = 'global'; var ctxEvalProbeObj = { n: 4 };");
  const seen = [ctxEvalProbe, globalThis.ctxEvalProbe, typeof ctxEvalProbeObj, ctxEvalProbeObj.n];
  const indirectFn = (0, eval)("(function () { return typeof ctxEvalProbe; })")();
  seen.push(indirectFn, delete globalThis.ctxEvalProbe, delete globalThis.ctxEvalProbeObj,
    typeof globalThis.ctxEvalProbe);
  return seen.join(",");
}

function deepReads(n) {
  let total = 0;
  let last = "";
  const top = { n: 1 };
  for (let i = 0; i < 3; i++) {
    let level1 = { n: 10 + i };
    {
      let level2 = { n: 100 };
      {
        const level3 = { n: 1000 };
        try {
          throw { n: 10000 };
        } catch (err) {
          const read = () => eval("top.n + level1.n + level2.n + level3.n + err.n");
          total += read();
          last = eval("typeof err + typeof level2");
          eval("level2 = { n: level2.n + " + n + " }");
          total += level2.n;
        }
      }
    }
  }
  return total + "," + last;
}

function constWrites() {
  const k = { n: 1 };
  const direct = outcome(() => eval("k = 2"));
  const compound = outcome(() => eval("k += 1"));
  const fromClosure = (() => outcome(() => eval("k = 3")))();
  const strict = outcome(() => eval("'use strict'; k = 4"));
  return [direct, compound, fromClosure, strict, k.n].join(",");
}

function selfBinding() {
  const sloppy = function self() {
    eval("self = 1");
    return typeof self;
  };
  const sloppyVar = function self2() {
    eval("var self2 = { n: 5 }");
    return typeof self2;
  };
  const strict = function self3() {
    "use strict";
    return outcome(() => eval("self3 = 1"));
  };
  const strictInEval = function self4() {
    return outcome(() => eval("'use strict'; self4 = 1"));
  };
  return [sloppy(), sloppyVar(), strict(), strictInEval()].join(",");
}

function hotSiblings(n) {
  function make(seed) {
    let a = { n: seed };
    let b = { n: seed * 2 };
    const incA = () => { a = { n: a.n + 1 }; };
    const readB = () => b.n;
    const both = () => a.n + b.n;
    if (seed % 1000 === 0) eval("var injected = { n: seed }");
    return [incA, readB, both, () => typeof injected === "undefined" ? 0 : injected.n];
  }
  let total = 0;
  for (let i = 0; i < n; i++) {
    const [incA, readB, both, inj] = make(i);
    incA();
    incA();
    total += both() - readB() + inj();
  }
  return total;
}

function run() {
  const lines = [];
  lines.push("sloppy=" + (function () { return this !== undefined; })());
  lines.push("shadow " + shadowAndDelete());
  lines.push("function " + evalFunction());
  lines.push("arrow " + arrowOwnsVarEnv());
  lines.push("nested " + nestedEval());
  lines.push("conflicts " + conflicts());
  lines.push("strict " + strictIsolation());
  lines.push("indirect " + indirectGlobal());
  lines.push("deep " + deepReads(3));
  lines.push("const " + constWrites());
  lines.push("self " + selfBinding());
  lines.push("hot " + hotSiblings(3000));
  for (const line of lines) console.log(line);
  return "context_eval_injection:" + lines.length;
}
run();
