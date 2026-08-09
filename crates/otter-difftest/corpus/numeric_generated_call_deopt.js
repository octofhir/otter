// A legacy-compiled caller enters a replacement-pipeline numeric callee through
// the universal generated-call frame. The hot callee initially publishes only
// its formal-parameter prefix; both checked overflow and an entry guard miss
// must expand the full window before interpreter resumption.

function numericLeaf(left, right) {
  const sum = left + right;
  const product = sum * right;
  const delta = product - right;
  const offset = delta + right;
  const scaled = offset * right;
  const reduced = scaled - right;
  const quotient = reduced / right;
  return -quotient;
}

function invoke(left, right) {
  return numericLeaf(left, right);
}

// Keep warmup calls interpreter-dispatched long enough for the callee to
// publish its optimizing generation before `invoke` compiles its direct edge.
eval("numericLeaf(2, 2);\n".repeat(12000));
eval("invoke(2, 2);\n".repeat(12000));

const checksum = invoke(2, 2);
const overflow = invoke(2147483647, 1);
const guardMiss = invoke("otter", 7);

JSON.stringify({ checksum, overflow, guardMiss: Number.isNaN(guardMiss) });
