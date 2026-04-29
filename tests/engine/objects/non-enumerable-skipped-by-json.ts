/* otter-test:
name = "object: non-enumerable own properties are skipped by JSON.stringify"
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
if (o.hidden !== 2) fail();
let json = JSON.stringify(o);
if (json !== "{\"visible\":1}") fail();
