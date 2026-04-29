/* otter-test:
name = "object: Object.defineProperties applies a bag of descriptors"
[expect]
exit_code = 0
*/
function fail() {
  return undefined.x;
}
let o = {};
Object.defineProperties(o, {
  a: { value: 1, writable: true, enumerable: true, configurable: true },
  b: { value: 2, writable: false, enumerable: true, configurable: false },
});
if (o.a !== 1) fail();
if (o.b !== 2) fail();
let descA = Object.getOwnPropertyDescriptor(o, "a");
let descB = Object.getOwnPropertyDescriptor(o, "b");
if (descA.writable !== true) fail();
if (descB.writable !== false) fail();
if (descB.configurable !== false) fail();
