/* otter-test:
name = "object: Object.defineProperty installs a data descriptor"
[expect]
exit_code = 0
*/
function fail() {
  return undefined.x;
}
let o = {};
Object.defineProperty(o, "x", { value: 42, writable: true, enumerable: true, configurable: true });
if (o.x !== 42) fail();
let desc = Object.getOwnPropertyDescriptor(o, "x");
if (desc.value !== 42) fail();
if (desc.writable !== true) fail();
if (desc.enumerable !== true) fail();
if (desc.configurable !== true) fail();
