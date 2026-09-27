// Derived-class instance fields are initialized right after `super()` binds
// `this`, whatever syntactic shape performs the call: an arrow, a nested
// block, or direct eval. Field initializers run in their own function scope:
// they cannot see constructor parameters or locals, and `new.target` inside
// them is `undefined`. A second `super()` does not re-run initializers after
// the ReferenceError.
function outcome(f) {
  try {
    const value = f();
    return value && typeof value === "object" ? JSON.stringify(value) : String(value);
  } catch (e) {
    return e.constructor.name;
  }
}

class Base {
  constructor() { this.base = true; }
}

function arrowSuper() {
  const log = [];
  class C extends Base {
    x = (log.push("field"), 42);
    y = { n: this.base ? 1 : 0 };
    constructor() {
      const init = () => super();
      log.push("before");
      init();
      log.push("after-super:" + this.x);
      this.seen = this.x;
    }
  }
  const o = new C();
  const nested = (() => {
    class N extends Base {
      x = 43;
      constructor() {
        const a = () => () => super();
        a()();
        this.seen = this.x;
      }
    }
    return new N().seen;
  })();
  return [o.seen, o.x, o.y && o.y.n, o.base, nested].join(":") + "|" + log.join(",");
}

function conditionalSuper(c) {
  class C extends Base {
    x = 7;
    constructor(flag) {
      if (flag) {
        super();
        this.seen = this.x;
      } else {
        super();
        this.other = true;
        this.seen = this.x;
      }
    }
  }
  class L extends Base {
    x = 8;
    constructor() {
      for (let i = 0; i < 1; i++) { super(); }
      this.seen = this.x;
    }
  }
  class T extends Base {
    x = 9;
    constructor() {
      try { super(); } finally { this.seen = this.x; }
    }
  }
  const o = new C(c);
  return [o.seen, o.x, o.other === true, new L().seen, new T().seen].join(":");
}

function evalSuper() {
  class C extends Base {
    x = 3;
    constructor() {
      eval("super()");
      this.seen = this.x;
    }
  }
  class A extends Base {
    x = 4;
    constructor() {
      (() => eval("super()"))();
      this.seen = this.x;
    }
  }
  const o = new C();
  const a = new A();
  return [o.seen, o.x, a.seen, a.x].join(":");
}

function parameterNames() {
  class C extends Base {
    x = typeof a === "undefined" ? "no-param" : a;
    constructor(a) {
      super();
    }
  }
  class D extends Base {
    x = local;
    constructor() {
      var local = 5;
      super();
    }
  }
  class E {
    x = typeof a === "undefined" ? "no-param" : a;
    constructor(a) {}
  }
  return [outcome(() => new C(5).x), outcome(() => new D().x), outcome(() => new E(6).x)].join(",");
}

function fieldNewTarget() {
  class C extends Base {
    x = () => new.target;
    y = new.target;
    constructor() {
      super();
    }
  }
  class B {
    x = () => new.target;
    y = new.target;
  }
  const c = new C();
  const b = new B();
  const show = (v) => typeof v === "function" ? "fn:" + v.name : String(v);
  return [show(c.x()), show(c.y), show(b.x()), show(b.y)].join(",");
}

function doubleSuper() {
  let runs = 0;
  class C extends Base {
    x = ++runs;
    constructor() {
      const init = () => super();
      init();
      try { init(); } catch (e) { this.err = e.constructor.name; }
    }
  }
  const o = new C();
  return [o.x, runs, o.err].join(":");
}

function hot(n) {
  class P extends Base {
    x = { n: 1 };
    constructor(i) {
      const init = () => super();
      if (i & 1) init();
      else super();
      this.early = this.x;
    }
  }
  let total = 0;
  let missing = 0;
  for (let i = 0; i < n; i++) {
    const p = new P(i);
    if (p.x) total += p.x.n;
    if (!p.early) missing++;
  }
  return total + ":" + missing;
}

function run() {
  const lines = [];
  lines.push("sloppy=" + (function () { return this !== undefined; })());
  lines.push("arrow " + outcome(arrowSuper));
  lines.push("conditional " + outcome(() => conditionalSuper(true)) + "," + outcome(() => conditionalSuper(false)));
  lines.push("eval " + outcome(evalSuper));
  lines.push("params " + parameterNames());
  lines.push("new-target " + outcome(fieldNewTarget));
  lines.push("double " + outcome(doubleSuper));
  lines.push("hot " + outcome(() => hot(3000)));
  for (const line of lines) console.log(line);
  return "derived_class_fields_super_shapes:" + lines.length;
}
run();
