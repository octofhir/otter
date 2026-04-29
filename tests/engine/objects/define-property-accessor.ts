/* otter-test:
name = "object: defineProperty accessor invokes getter and setter"
[expect]
exit_code = 0
*/
function fail() {
  return undefined.x;
}
let o = {};
let backing = 10;
Object.defineProperty(o, "x", {
  get: function () { return backing * 2; },
  set: function (v) { backing = v; },
  enumerable: true,
  configurable: true,
});
if (o.x !== 20) fail();
o.x = 50;
if (backing !== 50) fail();
if (o.x !== 100) fail();
let desc = Object.getOwnPropertyDescriptor(o, "x");
if (typeof desc.get !== "function") fail();
if (typeof desc.set !== "function") fail();
if (desc.enumerable !== true) fail();
if (desc.configurable !== true) fail();
