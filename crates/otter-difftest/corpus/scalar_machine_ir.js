// Tagged values cross the replacement scalar pipeline unchanged through entry,
// branch phis, ordinary returns, and the native frame's `this` binding.

function scalarIdentity(value) {
  return value;
}

function scalarChoose(left, right, a, b) {
  let selected;
  if (a < b) {
    selected = left;
  } else {
    selected = right;
  }
  return selected;
}

function scalarEmpty() {}

function scalarThis() {
  return this;
}

const receiver = { marker: "receiver", scalarThis };

// Tier the callees independently before their final tagged inputs are created.
eval(
  "scalarIdentity(null); scalarChoose(null, false, 1, 2); scalarEmpty(); receiver.scalarThis();\n".repeat(
    12000,
  ),
);

const left = { marker: "left" };
const right = { marker: "right" };
const identity = scalarIdentity(left);
const first = scalarChoose(left, right, 1, 2);
const second = scalarChoose(left, right, 2, 1);

JSON.stringify({
  identity: identity.marker,
  first: first.marker,
  second: second.marker,
  empty: scalarEmpty() === undefined,
  thisValue: receiver.scalarThis().marker,
});
