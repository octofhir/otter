/* otter-test:
name = "object: `in` walks own + proto chain"
[expect]
exit_code = 0
*/
function fail() {
  return undefined.x;
}
let parent = { inherited: 1 };
let child = Object.create(parent);
child.own = 2;
if ("own" in child !== true) fail();
if ("inherited" in child !== true) fail();
if ("missing" in child !== false) fail();
// Non-enumerable own props are still observable through `in`.
let o = {};
Object.defineProperty(o, "hidden", { value: 1, enumerable: false, configurable: true });
if ("hidden" in o !== true) fail();
// Arrays — indexed `in` is bounded by length.
let xs = [10, 20, 30];
if (0 in xs !== true) fail();
if (2 in xs !== true) fail();
if (3 in xs !== false) fail();
if ("length" in xs !== true) fail();
