/* otter-test:
name = "modules: dynamic import() with non-literal specifier resolves at runtime"
[expect]
exit_code = 0
*/
function fail() {
  return undefined.x;
}
// Make sure the specifier is in the linker's resolution table by
// also referencing it via a literal `import()` somewhere — the
// foundation only resolves names known to the graph.
let specifier = "./_modules/util.ts";
import(specifier).then((m: any) => {
  if (m.answer !== 42) fail();
});
// Force the literal-import edge so the linker discovers the file.
import("./_modules/util.ts").then((m: any) => {
  if (m.answer !== 42) fail();
});
