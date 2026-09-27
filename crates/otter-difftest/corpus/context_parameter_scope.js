// Functions with parameter expressions get a parameter scope separate from
// the body's variable scope: a body `var a` is a new binding initialized
// from the parameter, while closures in defaults keep seeing the parameter.
// Without a body redeclaration, parameter closures and the body share one
// binding. Covers `arguments` in defaults, direct eval in defaults (vars land
// outside the parameter scope; redeclaring a parameter is a SyntaxError),
// the function's own name in a default, generator and async variants,
// destructuring defaults capturing earlier parameters, and a hot loop.
function outcome(f) {
  try { return String(f()); } catch (e) { return e.constructor.name; }
}

function f(a, g = () => a) {
  var a = { v: 2 };
  const before = g();
  a = { v: 3 };
  return [a.v, before.v, g().v].join(":");
}
function h(x, y = () => x, set = (v) => { x = v; }) {
  x = { v: 5 };
  const first = y().v;
  set({ v: 6 });
  return first + ":" + y().v + ":" + x.v;
}
function separateVsShared() {
  return outcome(() => f({ v: 1 })) + "|" + outcome(() => h({ v: 0 }));
}

function argumentsInDefaults() {
  function viaDefault(a, b = arguments.length, c = () => arguments[0]) {
    arguments[0] = "changed";
    return [a, b, c()].join(":");
  }
  function arrowArguments(a, get = () => arguments) {
    var arguments;
    return get() === arguments ? "same" : "different";
  }
  function shadowed(a, get = () => arguments) {
    var arguments = { own: true };
    return [typeof get()[0], get() === arguments].join(":");
  }
  return [outcome(() => viaDefault("x")), outcome(() => viaDefault("x", "y")),
    outcome(() => arrowArguments(1)), outcome(() => shadowed(1))].join(",");
}

function evalInDefaults() {
  function e1(a = eval("var z = { n: 1 }"), b = () => z.n) {
    var before = z.n;
    z = { n: 2 };
    return [before, b(), typeof a].join(":");
  }
  function e2(a = eval("var a = 1")) { return a; }
  function e3(a, b = eval("a + 1")) { return b; }
  function e4(a = 1, b = () => eval("a")) { var a = 2; return [a, b()].join(":"); }
  function e5(a = eval("var q = 7"), b = () => q) {
    var q = 8;
    return [q, b()].join(":");
  }
  return [outcome(e1), outcome(e2), outcome(() => e3(4)), outcome(e4), outcome(e5)].join(",");
}

function selfName() {
  function fn(a = () => fn) { return a() === fn; }
  const expr = function named(a = () => named) { return a() === named; };
  const shadow = function named2(named2 = 5, get = () => named2) { return get(); };
  const assign = function named3(a = () => { named3 = 1; return typeof named3; }) { return a(); };
  return [outcome(fn), outcome(expr), outcome(shadow), outcome(assign)].join(",");
}

function generatorAndAsync() {
  function* gen(a, g = () => a) {
    var a = { v: "body" };
    yield g().v;
    yield a.v;
  }
  function* gen2(x, y = () => x) {
    x = { v: "shared" };
    yield y().v;
  }
  const gs = [outcome(() => [...gen({ v: "param" })].join(",")), outcome(() => [...gen2({ v: "orig" })].join(","))];
  async function asy(a, g = () => a) {
    var a = { v: "abody" };
    await null;
    return g().v + "/" + a.v;
  }
  async function asy2(x, y = () => x) {
    await null;
    x = { v: "ashared" };
    return y().v;
  }
  const pending = Promise.all([asy({ v: "aparam" }), asy2({ v: "x" })]);
  return [gs.join(","), pending];
}

function destructuringDefaults() {
  function d({ a }, [b] = [() => a], { c = () => a + b() } = {}) {
    a = a + 100;
    return c();
  }
  function e({ n } = { n: 1 }, m = () => n, [k = n * 2] = []) {
    var n = 50;
    return [n, m(), k].join(":");
  }
  const tdz = outcome(() => (function ({ x } = { x: y }, y) { return x; })());
  return [outcome(() => d({ a: 1 })), outcome(() => e()), outcome(() => e({ n: 3 })), tdz].join(",");
}

function hot(n) {
  function inner(a, g = () => a, s = (v) => { a = v; }) {
    var a = { v: a.v * 2 };
    s({ v: a.v + 1 });
    return a.v + g().v;
  }
  function inner2(a, g = () => a) {
    a = { v: a.v + 1 };
    return g().v;
  }
  let total = 0;
  let thrown = 0;
  for (let i = 0; i < n; i++) {
    try { total += inner({ v: i & 31 }); } catch (e) { thrown++; }
    total += inner2({ v: i & 7 });
  }
  return total + ":" + thrown;
}

function run() {
  const lines = [];
  lines.push("sloppy=" + (function () { return this !== undefined; })());
  lines.push("scopes " + separateVsShared());
  lines.push("arguments " + argumentsInDefaults());
  lines.push("eval " + evalInDefaults());
  lines.push("self " + selfName());
  const [gens, pending] = generatorAndAsync();
  lines.push("generators " + gens);
  lines.push("destructuring " + destructuringDefaults());
  lines.push("hot " + hot(3000));
  for (const line of lines) console.log(line);
  pending.then(
    (values) => console.log("async " + values.join(",")),
    (e) => console.log("async-error " + e.constructor.name));
  return lines.length;
}
run();
