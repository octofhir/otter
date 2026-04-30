/* otter-test:
name = "classes: private fields are namespaced per declaration"
[expect]
exit_code = 0
*/
function fail() {
  return undefined.x;
}
class Counter {
  #n = 0;
  bump() {
    this.#n = this.#n + 1;
    return this.#n;
  }
  static has(o: any) {
    return #n in o;
  }
}
let c = new Counter();
if (c.bump() !== 1) fail();
if (c.bump() !== 2) fail();
if (Counter.has(c) !== true) fail();
if (Counter.has({}) !== false) fail();

class Other {
  #n = 100;
  read() {
    return this.#n;
  }
  static has(o: any) {
    return #n in o;
  }
}
let o = new Other();
if (o.read() !== 100) fail();
// Different #n namespaces — Counter's has-probe rejects an Other.
if (Counter.has(o) !== false) fail();
if (Other.has(c) !== false) fail();

// Private method.
class Quoter {
  #lit = "[";
  #rit = "]";
  wrap(text: string) {
    return this.#join(text);
  }
  #join(text: string) {
    return this.#lit + text + this.#rit;
  }
}
let q = new Quoter();
if (q.wrap("hi") !== "[hi]") fail();
