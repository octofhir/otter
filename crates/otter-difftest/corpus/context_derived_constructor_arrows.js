// Arrows inside a derived constructor share its `this` binding: `super()`
// through an arrow binds it, an arrow reading `this` first throws
// ReferenceError, a second `super()` throws ReferenceError after the base
// constructor ran again, an escaped `super()` arrow still targets the
// original activation. Also nested arrows, `eval("super()")`, `super.m()`,
// `new.target`, field-initializer arrows over `this`, constructors returning
// objects, and a hot construction loop.
function outcome(f) {
  try {
    const value = f();
    return value && typeof value === "object" ? "obj:" + (value.tag || value.constructor.name) : String(value);
  } catch (e) {
    return e.constructor.name;
  }
}

let baseRuns = 0;
class Base {
  constructor(v) {
    baseRuns++;
    this.tag = "base" + (v === undefined ? "" : v);
    this.payload = { v };
  }
  m() { return "Base.m:" + this.tag; }
}

function basicOrder() {
  const log = [];
  let escapedInit = null;
  class Derived extends Base {
    constructor(v) {
      const init = () => super(v);
      const peek = () => this;
      log.push("peek-before:" + outcome(peek));
      const first = init();
      log.push("first:" + (first === this) + ":" + first.tag);
      const runsBefore = baseRuns;
      log.push("second:" + outcome(init) + ":" + (baseRuns - runsBefore));
      log.push("peek-after:" + outcome(peek));
      escapedInit = init;
    }
  }
  baseRuns = 0;
  const d = new Derived(1);
  const runsBeforeEscape = baseRuns;
  log.push("escaped:" + outcome(escapedInit) + ":" + (baseRuns - runsBeforeEscape));
  log.push("instance:" + d.tag + ":" + (d instanceof Derived));
  return log.join(",");
}

function deepArrow() {
  class Deep extends Base {
    constructor() {
      const a = () => () => () => super("deep");
      const bind = a()();
      bind();
      this.after = this.tag + "!";
    }
  }
  const d = new Deep();
  return d.after + ":" + d.payload.v;
}

function evalSuper() {
  class ViaEval extends Base {
    constructor(v) {
      const arrow = () => eval("super(v)");
      arrow();
      this.seen = eval("this.tag");
    }
  }
  class ViaEvalTwice extends Base {
    constructor() {
      const arrow = () => eval("super('e')");
      arrow();
      this.second = outcome(arrow);
    }
  }
  return new ViaEval("v").seen + "," + new ViaEvalTwice().second;
}

function superProperty() {
  class Method extends Base {
    constructor() {
      const callM = () => super.m();
      const early = outcome(callM);
      super("m");
      this.early = early;
      this.late = callM();
    }
    m() { return "Method.m"; }
  }
  const o = new Method();
  return o.early + "," + o.late + "," + o.m();
}

function newTarget() {
  class Target extends Base {
    constructor() {
      const nt = () => new.target;
      const init = () => super(nt() === Target ? "direct" : "sub");
      init();
      this.nt = nt;
    }
  }
  class Sub extends Target {}
  const t = new Target();
  const s = new Sub();
  return [t.tag, s.tag, t.nt() === Target, s.nt() === Sub].join(",");
}

function fieldArrows() {
  class Fielded extends Base {
    self = () => this;
    tagCopy = this.tag + "+";
    constructor(v) {
      super(v);
      this.check = this.self() === this;
    }
  }
  const f = new Fielded("f");
  return [f.check, f.tagCopy, f.self().tag].join(",");
}

function returnsObject() {
  const replacement = { tag: "replacement" };
  class Replacing extends Base {
    constructor(mode) {
      const init = () => super(mode);
      if (mode === "skip") return replacement;
      init();
      if (mode === "obj") return { tag: "returned", inner: this.tag };
      if (mode === "prim") return 5;
    }
  }
  class NoSuper extends Base {
    constructor() { const later = () => super(); }
  }
  return [outcome(() => new Replacing("skip")), outcome(() => new Replacing("obj")),
    outcome(() => new Replacing("prim")), outcome(() => new NoSuper())].join(",");
}

function hotConstruct(n) {
  class Point extends Base {
    constructor(x, y) {
      const init = () => super(x);
      const get = () => this.payload.v + y;
      if ((x & 1023) === 0) {
        try { get(); } catch (e) { thisErrors++; }
      }
      init();
      this.sum = get();
    }
  }
  let thisErrors = 0;
  let total = 0;
  const kept = [];
  for (let i = 0; i < n; i++) {
    const p = new Point(i, 2);
    total += p.sum;
    if (i % 1000 === 0) kept.push(p);
  }
  return total + ":" + thisErrors + ":" + kept.map(p => p.tag).join("|");
}

function run() {
  const lines = [];
  lines.push("sloppy=" + (function () { return this !== undefined; })());
  lines.push("order " + basicOrder());
  lines.push("deep " + deepArrow());
  lines.push("eval " + evalSuper());
  lines.push("super-property " + superProperty());
  lines.push("new-target " + newTarget());
  lines.push("fields " + fieldArrows());
  lines.push("returns " + returnsObject());
  lines.push("hot " + hotConstruct(3000));
  for (const line of lines) console.log(line);
  return "context_derived_constructor_arrows:" + lines.length;
}
run();
