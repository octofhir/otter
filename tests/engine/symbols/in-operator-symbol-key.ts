/* otter-test:
name = "symbol: `in` operator finds symbol-keyed own property"
[expect]
exit_code = 0
*/
function fail() {
  return undefined.x;
}
let s = Symbol("tag");
let o: any = {};
o[s] = 42;
if ((s in o) !== true) fail();
let other = Symbol("other");
if ((other in o) !== false) fail();
