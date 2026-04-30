/* otter-test:
name = "exceptions: implicit VM errors surface as typed Error instances"
[expect]
exit_code = 0
*/
function fail() {
  return undefined.x;
}
let kind = "";
try {
  let o: any = null;
  let r = o.something;
} catch (e: any) {
  if (e instanceof TypeError) kind = "type";
  else if (e instanceof Error) kind = "error";
  else kind = "other";
}
if (kind !== "type") fail();

// Calling a non-function — TypeError.
let kind2 = "";
try {
  let v: any = 42;
  v();
} catch (e: any) {
  if (e instanceof TypeError) kind2 = "type";
}
if (kind2 !== "type") fail();

// Throwables carry a `.message`.
let msg = "";
try {
  let o: any = undefined;
  o.x;
} catch (e: any) {
  msg = e.message;
}
if (msg.length === 0) fail();
