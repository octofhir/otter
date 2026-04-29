/* otter-test:
name = "object: Object.assign copies own enumerable props from each source"
[expect]
exit_code = 0
*/
function fail() {
  return undefined.x;
}
let target = { a: 1 };
let result = Object.assign(target, { b: 2 }, { c: 3, a: 9 });
if (result !== target) fail();
if (target.a !== 9) fail();
if (target.b !== 2) fail();
if (target.c !== 3) fail();
// null / undefined sources are silently skipped per spec.
Object.assign(target, null, undefined, { d: 4 });
if (target.d !== 4) fail();
