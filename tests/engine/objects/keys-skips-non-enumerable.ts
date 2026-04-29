/* otter-test:
name = "object: Object.keys skips non-enumerable, getOwnPropertyNames keeps them"
[expect]
exit_code = 0
*/
function fail() {
  return undefined.x;
}
let o = { visible: 1 };
Object.defineProperty(o, "hidden", {
  value: 2,
  writable: true,
  enumerable: false,
  configurable: true,
});
let keys = Object.keys(o);
if (keys.length !== 1) fail();
if (keys[0] !== "visible") fail();
let all = Object.getOwnPropertyNames(o);
if (all.length !== 2) fail();
if (all[0] !== "visible") fail();
if (all[1] !== "hidden") fail();
