// Straight-line method bodies exercise compact inline scratch planning. The
// observable result must match the interpreter after argument/local slot
// reuse, read-before-write initialization, and parameter-assignment snapshots.

function compactApply(left, right) {
  let sum = left + right;
  return sum + this.bias;
}

function snapshotApply(value) {
  return value + (value = 2) + this.bias;
}

function hoistedApply() {
  var missing;
  return missing;
}

function callCompact(receiver, left, right) {
  return receiver.compact(left, right);
}

function callSnapshot(receiver, value) {
  return receiver.snapshot(value);
}

function callHoisted(receiver) {
  return receiver.hoisted();
}

const receiver = {
  bias: 4,
  compact: compactApply,
  snapshot: snapshotApply,
  hoisted: hoistedApply,
};

for (let i = 0; i < 5000; i++) {
  callCompact(receiver, i, 2);
  callSnapshot(receiver, i);
  callHoisted(receiver);
}

JSON.stringify([
  callCompact(receiver, 3, 5),
  callSnapshot(receiver, 9),
  callHoisted(receiver),
]);
