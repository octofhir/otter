// A list walk whose loop header is the function's own first instruction. The
// frame entry carries the seed definitions of every register, so a back edge
// into it has to merge the latch's value rather than let the parameter seed win
// again on each iteration — otherwise the walk never advances and never returns.

function Pair(car, cdr) {
  this.car = car;
  this.cdr = cdr;
}

function walkList(node, key) {
  while (node !== null) {
    if (node.car === key) return node;
    node = node.cdr;
  }
  return false;
}

function driver(list, rounds) {
  let hits = 0;
  for (let i = 0; i < rounds; i++) {
    if (walkList(list, "c") !== false) hits++;
    if (walkList(list, "absent") === false) hits++;
  }
  return hits;
}

let list = null;
for (let i = 0; i < 4; i++) list = new Pair("abcd".charAt(i), list);

let total = 0;
for (let round = 0; round < 400; round++) total += driver(list, 1000);

const tail = walkList(list, "c");

JSON.stringify({ total, tail: tail === false ? null : tail.car });
