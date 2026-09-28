// A context value that regalloc2 keeps in a callee-saved register and in its
// spill slot at once: the loop body reloads it into a register for a store,
// the allocating copy call names only the register as its root, and the edge
// into the inner loop's preheader skips its register-to-slot move because the
// slot "already holds" the value. A moving collection during the call must
// rewrite both copies, or the inner loop starts from a stale context and
// copies the wrong scope.
function P(a, b) { this.a = a; this.b = b; }
function cons(a, b) { return new P(a, b); }
let total = 0;
for (let round = 0; round < 3000; round++) {
  let list = null;
  for (let i = 0; i < 40; i++) list = cons(i, list);
  total = (total + list.a) | 0;
}
console.log(total);
