/* otter-test:
name = "exceptions: Error.prototype.toString — name + message"
[expect]
exit_code = 0
*/
function fail() {
  return undefined.x;
}
// Subclass with message.
let t: any = new TypeError("bad arg");
if (t.toString() !== "TypeError: bad arg") fail();
let r: any = new RangeError("out of range");
if (r.toString() !== "RangeError: out of range") fail();
// Bare Error — name only.
let e: any = new Error();
if (e.toString() !== "Error") fail();
// Custom shape with only message — toString uses message alone.
let only: any = { message: "boom" };
if (only.toString() !== "boom") fail();
// Custom shape with only name.
let nameOnly: any = { name: "Custom" };
if (nameOnly.toString() !== "Custom") fail();
// Plain object falls back to [object Object].
let plain: any = { x: 1 };
if (plain.toString() !== "[object Object]") fail();
