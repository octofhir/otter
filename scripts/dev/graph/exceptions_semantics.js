// try/catch/finally, abrupt completions through finally, iterator close
// ordering, generator return/throw through finally.
const log = [];
function f1() { try { return 1; } finally { log.push("f1"); } }
log.push(f1());
function f2() { try { throw 2; } catch (e) { return e; } finally { log.push("f2"); } }
log.push(f2());
function f3() { for (let i = 0; i < 3; i++) { try { if (i === 1) continue; if (i === 2) break; log.push("b" + i); } finally { log.push("fin" + i); } } return "f3"; }
log.push(f3());
function f4() { try { try { throw "x"; } finally { log.push("inner"); } } catch (e) { return "caught " + e; } }
log.push(f4());
function f5() { try { return "a"; } finally { return "b"; } }
log.push(f5());
function f6() { outer: for (const a of [1, 2]) { for (const b of [3, 4]) { try { continue outer; } finally { log.push("f6:" + a + b); } } } return "f6"; }
log.push(f6());
const it = { [Symbol.iterator]() { let n = 0; return { next() { return { value: n++, done: n > 5 }; }, return() { log.push("ret"); return {}; } }; } };
for (const x of it) { if (x === 2) break; }
try { for (const x of it) { throw "boom"; } } catch (e) { log.push("c:" + e); }
function f7() { for (const x of it) { return x; } }
log.push(f7());
const bad = { [Symbol.iterator]() { return { next() { return { value: 1, done: false }; }, return() { throw "retErr"; } }; } };
try { for (const x of bad) { try { break; } catch (e) { log.push("wrong:" + e); } } } catch (e) { log.push("outer:" + e); }
try { for (const x of bad) { throw "orig"; } } catch (e) { log.push("orig kept:" + e); }
function* g() { try { yield 1; yield 2; } finally { log.push("gfin"); } }
const gi = g(); gi.next(); log.push(JSON.stringify(gi.return(9)));
function* g2() { try { yield 1; } catch (e) { log.push("gc:" + e); yield 3; } }
const g2i = g2(); g2i.next(); log.push(JSON.stringify(g2i.throw("t")));
function* g3() { try { yield 1; } finally { yield "inf"; } }
const g3i = g3(); g3i.next(); log.push(JSON.stringify(g3i.return(5))); log.push(JSON.stringify(g3i.next()));
let [a, b] = it; log.push(a, b);
const nextThrows = { [Symbol.iterator]() { return { next() { throw "nt"; }, return() { log.push("WRONG close"); } }; } };
try { for (const x of nextThrows) {} } catch (e) { log.push("nt:" + e); }
try { let [p] = nextThrows; } catch (e) { log.push("dnt:" + e); }
class A { constructor() { try { return; } finally { log.push("ctor fin"); } } }
class B extends A { constructor() { try { super(); } finally { log.push("derived fin"); } } }
new B();
async function af() { try { await Promise.reject("rej"); } catch (e) { log.push("af:" + e); } finally { log.push("af fin"); } return "afr"; }
af().then(v => { log.push(v); console.log(log.join(",")); });
