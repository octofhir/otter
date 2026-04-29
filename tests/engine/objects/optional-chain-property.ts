/* otter-test:
name = "object: optional chaining short-circuits on null/undefined"
[expect]
exit_code = 0
*/
function fail() {
  return undefined.x;
}
let o: any = { a: { b: { c: 42 } } };
if (o?.a?.b?.c !== 42) fail();
let missing: any = null;
if (missing?.a?.b !== undefined) fail();
let undef: any = undefined;
if (undef?.x?.y !== undefined) fail();
// Optional skips remaining evaluation: side-effect should not run.
let counter = 0;
function bump() {
  counter = counter + 1;
  return { x: 1 };
}
let nope: any = null;
let r = nope?.[bump().x];
if (r !== undefined) fail();
if (counter !== 0) fail();
