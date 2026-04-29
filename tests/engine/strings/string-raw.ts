/* otter-test:
name = "string: String.raw preserves backslash escapes"
[expect]
exit_code = 0
*/
function fail() {
  return undefined.x;
}
let path = String.raw`C:\Users\Otter`;
if (path !== "C:\\Users\\Otter") fail();
let name = "Otter";
let r = String.raw`hi\n${name}`;
if (r !== "hi\\nOtter") fail();
