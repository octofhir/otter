// Sloppy mapped arguments alias captured formals in both directions: a write
// through `arguments[i]` is seen by a closure over the formal and a closure
// write is seen through `arguments[i]`, also after the activation returned.
// `delete` and a non-writable redefinition unmap an index; duplicate formals
// map only the last occurrence; indices past the actual count are never
// mapped; generators keep the mapping across `yield`; a default parameter
// makes the object unmapped; eval writes through the same map.
function outcome(f) {
  try { return String(f()); } catch (e) { return e.constructor.name; }
}

function captured(a, b) {
  const read = () => a.n + b.n;
  const write = (v) => { b = { n: v }; };
  return { args: arguments, read, write };
}

function hotAliasing(n) {
  let total = 0;
  let check = "";
  for (let i = 0; i < n; i++) {
    const s = captured({ n: i }, { n: 1 });
    s.args[0] = { n: 2 * i };
    total += s.read();
    s.write(i & 15);
    total += s.args[1].n;
    if (i % 1000 === 0) check += s.read() + ":" + s.args[1].n + ";";
  }
  return total + "|" + check;
}

function unmapping() {
  function viaDelete(a, b) {
    const read = () => [a, b].join("");
    delete arguments[0];
    arguments[0] = "X";
    a = "A";
    arguments[1] = "Y";
    return read() + "/" + arguments[0] + arguments[1];
  }
  function viaDefine(a, b) {
    const read = () => [a, b].join("");
    Object.defineProperty(arguments, "0", { value: "D", writable: false });
    const afterDefine = read();
    a = "A";
    b = "B";
    return afterDefine + "/" + read() + "/" + arguments[0] + arguments[1] + "/" +
      outcome(() => { "use strict"; arguments[0] = "Z"; });
  }
  function viaAccessor(a) {
    Object.defineProperty(arguments, "0", { get() { return "G"; } });
    a = "A";
    return a + arguments[0];
  }
  function viaDefineWritable(a) {
    const read = () => a;
    Object.defineProperty(arguments, "0", { value: "W" });
    const seen = read();
    arguments[0] = "V";
    return seen + read() + arguments[0];
  }
  return [viaDelete("a", "b"), viaDefine("a", "b"), viaAccessor("a"), viaDefineWritable("a")].join(",");
}

function escaped() {
  function leak(a, b) {
    const get = () => a.tag + b;
    const set = (v) => { a = { tag: v }; };
    return [arguments, get, set];
  }
  const [args, get, set] = leak({ tag: "x" }, 1);
  args[0] = { tag: "y" };
  args[1] = 2;
  const afterArgs = get();
  set("z");
  return afterArgs + "," + get() + "," + args[0].tag + "," + args.length;
}

function duplicates() {
  function dup(a, a) {
    const read = () => a;
    arguments[0] = "first";
    const r1 = read();
    arguments[1] = "second";
    const r2 = read();
    a = "third";
    return [r1, r2, arguments[0], arguments[1]].join(",");
  }
  return dup("p", "q") + "|" + dup("p");
}

function fewerActuals() {
  function few(a, b, c) {
    const read = () => [a, b, c].join(":");
    arguments[1] = "B";
    arguments[2] = "C";
    b = "b2";
    c = "c2";
    return read() + "/" + arguments.length + "/" + arguments[1] + "/" + arguments[2];
  }
  return few("a") + "|" + few("a", "b");
}

function generatorMapping() {
  function* gen(a, b) {
    const read = () => a + b;
    arguments[0] = 10;
    const x = yield read();
    arguments[1] = x;
    const y = yield read();
    a = y;
    yield arguments[0] + arguments[1];
    return read();
  }
  const it = gen(1, 2);
  const out = [];
  out.push(it.next().value, it.next(20).value, it.next(5).value, it.next().value);
  return out.join(",");
}

function unmappedContrast() {
  function withDefault(a, b = 2) {
    const read = () => a + b;
    arguments[0] = 100;
    a = 1;
    return read() + "," + arguments[0];
  }
  function withRest(a, ...rest) {
    arguments[0] = 100;
    return a + rest.length;
  }
  function withPattern(a, { n }) {
    arguments[0] = 100;
    return a + n;
  }
  function strict(a) {
    "use strict";
    arguments[0] = 100;
    return a;
  }
  return [withDefault(1), withRest(1, 2), withPattern(1, { n: 2 }), strict(1)].join("|");
}

function evalWrites() {
  function viaEval(a, b) {
    const read = () => a + "/" + b.n;
    eval("arguments[0] = 'e0'");
    const r1 = read();
    eval("b = { n: 9 }");
    const r2 = arguments[1].n;
    eval("arguments[1] = { n: 11 }");
    return [r1, r2, read()].join(",");
  }
  function evalLength(a) {
    eval("arguments.length = 0");
    arguments[0] = "gone";
    return a;
  }
  return viaEval("a", { n: 1 }) + "|" + evalLength("kept");
}

function run() {
  const lines = [];
  lines.push("sloppy=" + (function () { return this !== undefined; })());
  lines.push("hot " + hotAliasing(3000));
  lines.push("unmap " + unmapping());
  lines.push("escaped " + escaped());
  lines.push("duplicates " + duplicates());
  lines.push("fewer " + fewerActuals());
  lines.push("generator " + generatorMapping());
  lines.push("unmapped " + unmappedContrast());
  lines.push("eval " + evalWrites());
  for (const line of lines) console.log(line);
  return "context_mapped_arguments:" + lines.length;
}
run();
