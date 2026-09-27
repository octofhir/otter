// Every evaluation of a class body creates fresh private names, so closures
// made by one evaluation only reach instances branded by that evaluation.
// Covers arrows over `this.#x`, cross-evaluation brand checks and `#x in o`,
// private methods and accessors captured by arrows, `static {}` blocks
// closing over class-scope bindings, the immutable inner class name, a
// nested class shadowing a private name, and a hot captured getter.
function outcome(f) {
  try { return String(f()); } catch (e) { return e.constructor.name; }
}

function makeClass(seed) {
  let created = 0;
  return class Counter {
    #x;
    #hidden = { seed };
    static #count = 0;
    static reader;
    static {
      Counter.reader = (o) => o.#x.n;
      this.bump = () => ++Counter.#count + created;
    }
    constructor(n) {
      this.#x = { n };
      created++;
      this.get = () => this.#x.n;
      this.set = (v) => { this.#x = { n: v }; };
    }
    #double() { return this.#x.n * 2; }
    get #view() { return "view" + this.#x.n; }
    set #view(v) { this.#x = { n: v.length }; }
    tools() {
      return {
        double: () => this.#double(),
        view: () => this.#view,
        write: (s) => { this.#view = s; },
      };
    }
    static has(o) { return #x in o; }
    static peek(o) { return o.#hidden.seed; }
    static rename() { Counter = null; }
  };
}

function distinctEvaluations() {
  const classes = [];
  for (let i = 0; i < 4; i++) classes.push(makeClass(i));
  const [A, B] = classes;
  const a = new A(1);
  const b = new B(2);
  const results = [
    a.get(), b.get(), A.reader(a), outcome(() => A.reader(b)), outcome(() => B.reader(a)),
    A.has(a), A.has(b), B.has(b), A.has({}), outcome(() => A.has(1)),
    A.peek(a), B.peek(b), outcome(() => A.peek(b)),
  ];
  a.set(10);
  results.push(a.get(), A.reader(a), b.get());
  const brands = classes.map((C, i) => classes.map((D) => D.has(new C(i)) ? 1 : 0).join("")).join("/");
  return results.join(",") + "|" + brands;
}

function methodsAndAccessors() {
  const C = makeClass(7);
  const o = new C(3);
  const t = o.tools();
  const before = [t.double(), t.view()];
  t.write("abcde");
  const foreign = new (makeClass(8))(4);
  const stolen = o.tools.call(foreign);
  return before.concat([t.double(), t.view(), o.get(), outcome(() => stolen.double()),
    outcome(() => stolen.view()), outcome(() => stolen.write("zz"))]).join(",");
}

function staticBlocks() {
  const C = makeClass(1);
  const D = makeClass(1);
  new C(1);
  new C(2);
  const out = [C.bump(), C.bump(), D.bump()];
  const log = [];
  class S {
    static base = { v: 5 };
    static fns = [];
    static {
      let local = { v: 1 };
      S.fns.push(() => local.v + S.base.v);
      S.fns.push(() => { local = { v: local.v + 10 }; });
      log.push(typeof this === "function" && this === S);
    }
    static {
      S.fns.push(() => typeof local);
    }
  }
  S.fns[1]();
  out.push(S.fns[0](), S.fns[2](), log[0]);
  return out.join(",");
}

function innerName() {
  const C = makeClass(0);
  const renamed = outcome(() => C.rename());
  class Outer {
    static tryAssign() { Outer = 1; }
    static capture = () => Outer;
  }
  const inner = outcome(() => Outer.tryAssign());
  const captured = Outer.capture() === Outer;
  const decl = (function () {
    class K { static self() { return K; } }
    const original = K;
    K = "outer-rebound";
    return [K, original.self() === original].join(":");
  })();
  const expr = (function () {
    const E = class Named { static assign() { "use strict"; Named = 0; } static sloppy() { return Named; } };
    return [outcome(() => E.assign()), E.sloppy() === E].join(":");
  })();
  return [renamed, inner, captured, decl, expr].join(",");
}

function nestedShadow() {
  class Outer {
    #p = "outer";
    static make() {
      class Inner {
        #p = "inner";
        static read(o) { return o.#p; }
      }
      return Inner;
    }
    static read(o) { return o.#p; }
  }
  const Inner = Outer.make();
  const o = new Outer();
  const i = new Inner();
  return [Outer.read(o), Inner.read(i), outcome(() => Inner.read(o)), outcome(() => Outer.read(i))].join(",");
}

function hotGetter(n) {
  const C = makeClass(3);
  const objs = [];
  for (let i = 0; i < 16; i++) objs.push(new C(i));
  const getters = objs.map((o) => o.get);
  const readers = objs.map((o) => () => C.reader(o));
  let total = 0;
  for (let i = 0; i < n; i++) {
    const k = i & 15;
    total += getters[k]() + readers[k]();
    if ((i & 255) === 0) objs[k].set(objs[k].get() + 1);
  }
  const Other = makeClass(4);
  const stranger = new Other(1);
  return total + "," + outcome(() => C.reader(stranger));
}

function run() {
  const lines = [];
  lines.push("sloppy=" + (function () { return this !== undefined; })());
  lines.push("distinct " + distinctEvaluations());
  lines.push("methods " + methodsAndAccessors());
  lines.push("static " + staticBlocks());
  lines.push("inner-name " + innerName());
  lines.push("nested " + nestedShadow());
  lines.push("hot " + hotGetter(3000));
  for (const line of lines) console.log(line);
  return "context_class_private_names:" + lines.length;
}
run();
