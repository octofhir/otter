/* otter-test:
name = "boolean: Boolean(value) coercion + prototype methods"
[expect]
exit_code = 0
*/
function fail() {
  return undefined.x;
}
// Boolean(value) — primitive coercion shape.
if (Boolean(0) !== false) fail();
if (Boolean(1) !== true) fail();
if (Boolean(NaN) !== false) fail();
if (Boolean("") !== false) fail();
if (Boolean("0") !== true) fail();
if (Boolean(null) !== false) fail();
if (Boolean(undefined) !== false) fail();
if (Boolean({}) !== true) fail();
if (Boolean([]) !== true) fail();
if (Boolean() !== false) fail();
// `new Boolean(value)` — foundation aliases to ToBoolean.
if (new Boolean(true) !== true) fail();
if (new Boolean(0) !== false) fail();
// Prototype methods.
if (true.toString() !== "true") fail();
if (false.toString() !== "false") fail();
if (true.valueOf() !== true) fail();
if (false.valueOf() !== false) fail();
