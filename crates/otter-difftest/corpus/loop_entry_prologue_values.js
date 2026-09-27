// Functions whose first bytecode is a loop header: the Machine entry is a
// separate prologue, so loop-carried parameters, locals first read as
// `undefined`, and typed arguments all merge with their back-edge values.
function sumDown(n, step) {
  while (n > 0) {
    var acc;
    acc = (acc === undefined ? 0 : acc) + n;
    n -= step;
    if (n <= 0) return acc;
  }
  return -1;
}

function countBits(x) {
  do {
    var count = (count | 0) + (x & 1);
    x >>>= 1;
  } while (x !== 0);
  return count;
}

function lastNode(node) {
  while (node.next !== null) node = node.next;
  return node.value;
}

let list = { value: 0, next: null };
for (let i = 1; i < 6; i++) list = { value: i, next: list };

let out = 0;
for (let i = 0; i < 20000; i++) {
  out += sumDown(40 + (i & 7), 3);
  out += countBits(i * 2654435761 >>> 0);
  out += lastNode(list);
}
console.log(out, sumDown(10, 4), countBits(0xffffffff), lastNode(list));
