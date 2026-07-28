// A nested monomorphic method chain reaches an upvalue operation in its
// innermost body. The optimizing tier must rebuild every spliced frame at its
// exact PC without replaying the outer caller's already-observed increment.

let outerEffects = 0;
let innerEffects = 0;

function leaf(value) {
  innerEffects += value;
  return this.bias + innerEffects;
}

function middle(value) {
  return this.leaf(value);
}

function dispatch(receiver, iterations) {
  let result = 0;
  for (let i = 0; i < iterations; i++) {
    outerEffects++;
    result = receiver.middle(1);
  }
  return result;
}

const receiver = {
  bias: 7,
  leaf,
  middle,
};

const result = dispatch(receiver, 50000);

JSON.stringify({ outerEffects, innerEffects, result });
