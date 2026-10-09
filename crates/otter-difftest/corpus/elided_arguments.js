// Elided `arguments.length` / `arguments[i]` in bodies that are spliced
// into their callers and in bodies that run on their own: dynamic and
// constant indices, out-of-range and non-index keys, mapped formals that
// were reassigned, strict bodies, and a deopt inside a spliced body.
function list() { let r = null; const a = arguments; for (let i = a.length - 1; i >= 0; i--) r = { car: a[i], cdr: r }; return r; }
function len(l) { let n = 0; while (l !== null) { n++; l = l.cdr; } return n; }
function sum() { let s = 0; for (let i = 0; i < arguments.length; i++) s += arguments[i]; return s; }
function last() { return arguments[arguments.length - 1]; }
function past() { return arguments[arguments.length]; }
function keyed(k) { return arguments[k]; }
function mapped(a, b) { a = 10; return arguments[0] + arguments[1]; }
function strictMapped(a) { "use strict"; a = 5; return arguments[0]; }
function first() { return arguments[0]; }
const out = [];
let acc = 0;
for (let round = 0; round < 3000; round++) {
  acc += len(list(1, 2, 3)) + len(list(round, round)) + len(list());
  acc += sum(1, 2, 3, 4) + sum(round) + sum();
  acc += last(1, 2, round) + (past(1, 2) === undefined ? 1 : 0);
  acc += (keyed(1, 7) | 0) + (keyed("length", 1, 2) | 0) + (keyed(-1, 3) === undefined ? 2 : 0);
  acc += mapped(1, 2) + strictMapped(3) + (first() === undefined ? 4 : 0);
  // A huge call keeps its body out of line; the values still read.
  acc += sum(1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11);
  // A double leaves the spliced body once.
  acc += round === 2500 ? sum(1.5, 2.25) : 0;
  acc = acc | 0;
}
out.push(acc);
Object.prototype[3] = "proto";
out.push(keyed(3, 1), past(1, 2, 3));
delete Object.prototype[3];
function callee() { return arguments.callee === callee; }
out.push(callee(), keyed("callee") === keyed);
console.log(out.join(","));
