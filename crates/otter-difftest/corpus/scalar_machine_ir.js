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

function scalarTruth(condition, left, right) {
  return condition ? left : right;
}

function scalarNot(value) {
  return !value;
}

function scalarStrictEq(left, right) {
  return left === right;
}

function scalarStrictNe(left, right) {
  return left !== right;
}

function scalarConcat3(left, middle, right) {
  return (left + middle) + right;
}

function scalarConcatInt(prefix, value, suffix) {
  return (prefix + value) + suffix;
}

function scalarConcatMiss(left, middle, right) {
  return (left + middle) + right;
}

const receiver = { marker: "receiver", scalarThis };

// Tier the callees independently before their final tagged inputs are created.
eval(
  "scalarIdentity(null); scalarChoose(null, false, 1, 2); scalarEmpty(); receiver.scalarThis();\n".repeat(
    12000,
  ),
);
eval("scalarTruth(true, null, false); scalarNot(false);\n".repeat(12000));
eval("scalarStrictEq(null, null); scalarStrictNe(null, false);\n".repeat(12000));
eval(
  'scalarConcat3("otter-", "machine-", "ir"); scalarConcatInt("key", 42, "!"); scalarConcatMiss("a", "b", "c");\n'.repeat(
    12000,
  ),
);

const left = { marker: "left" };
const right = { marker: "right" };
const identity = scalarIdentity(left);
const first = scalarChoose(left, right, 1, 2);
const second = scalarChoose(left, right, 2, 1);
const objectTruth = scalarTruth({ marker: "object" }, left, right);
const stringTruth = scalarTruth("otter", left, right);
const emptyStringTruth = scalarTruth("", left, right);
const equalString = scalarStrictEq("otter", "otter");
const concat = scalarConcat3("otter-", "machine-", "ir");
const concatInt = scalarConcatInt("key", 42, "!");
let concatCoercions = 0;
const concatMiss = scalarConcatMiss(
  {
    toString() {
      concatCoercions++;
      return "object";
    },
  },
  "-",
  "tail",
);

JSON.stringify({
  identity: identity.marker,
  first: first.marker,
  second: second.marker,
  empty: scalarEmpty() === undefined,
  thisValue: receiver.scalarThis().marker,
  objectTruth: objectTruth.marker,
  stringTruth: stringTruth.marker,
  emptyStringTruth: emptyStringTruth.marker,
  notObject: scalarNot(left),
  notEmptyString: scalarNot(""),
  notNan: scalarNot(0 / 0),
  sameObject: scalarStrictEq(left, left),
  differentObject: scalarStrictEq(left, right),
  equalString,
  nanStrictEqual: scalarStrictEq(0 / 0, 0 / 0),
  mixedStrictEqual: scalarStrictEq(1, true),
  strictNotEqual: scalarStrictNe(left, right),
  concat,
  concatInt,
  concatMiss,
  concatCoercions,
});
