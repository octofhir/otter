/* otter-test:
name = "modules: import.meta.resolve joins relative specifiers"
[expect]
exit_code = 0
*/
function fail() {
  return undefined.x;
}
let r = import.meta.resolve("./_modules/util.ts");
// The result must be an absolute file:// URL that ends with the
// resolved suffix.
if (typeof r !== "string") fail();
if (r.indexOf("util.ts") < 0) fail();
if (r.indexOf("import-meta-resolve") < 0) fail();
// Absolute https:// passes through unchanged.
let abs = import.meta.resolve("https://example.com/x.js");
if (abs !== "https://example.com/x.js") fail();
