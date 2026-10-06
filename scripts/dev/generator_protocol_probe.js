// Replaced generator protocol methods are observed (§7.4.2, §7.4.11).
console.log("start");
const log = [];
function* g() { try { yield 1; yield 2; yield 3; } finally { log.push("finally"); } }
const GP = Object.getPrototypeOf(g.prototype);
const next = GP.next;
const ret = GP.return;
for (let round = 0; round < 3; round++) {
  log.length = 0;
  for (const x of g()) { log.push(x); if (x === 2) break; }
  log.push([...g()].length);
  GP.next = function () { log.push("n"); return next.call(this); };
  for (const x of g()) log.push(x);
  const [a] = g();
  log.push(a);
  GP.next = next;
  GP.return = function () { log.push("r"); return ret.call(this); };
  for (const x of g()) break;
  GP.return = ret;
  const own = g();
  own.next = function () { log.push("own"); return { done: true }; };
  for (const x of own) log.push(x);
  g.prototype.next = function () { log.push("fnproto"); return { done: true }; };
  for (const x of g()) log.push(x);
  delete g.prototype.next;
  log.push(new Set(g()).size);
}
console.log(log.join(","));
