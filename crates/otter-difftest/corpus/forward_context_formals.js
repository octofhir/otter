// A sloppy forwarder whose formals are mapped (so they live in a context
// slot) is hot enough for the optimizing tier. The native forward reads the
// incremented formal from the context; an overridden `apply` then receives a
// materialized mapped arguments object that aliases the same slots, so its
// write to `list[0]` is visible through the formal after the call.
function target(a) { return a + arguments.length; }
let selected = target;
function forward(a, b) {
  a++;
  const result = selected.apply(null, arguments);
  return result + ":" + a;
}
let warm = "";
for (let i = 0; i < 3000; i++) warm = forward(i, 2);
const custom = function (a) { return a; };
custom.apply = function (receiver, list) {
  const seen = list[0] * 10 + list.length;
  list[0] = -1;
  return seen;
};
selected = custom;
let last = "";
for (let i = 0; i < 128; i++) last = forward(i, 2, 3);
console.log("forward_context_formals", warm, last);
