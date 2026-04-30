/* otter-test:
name = "modules: import x from `./y.json` exposes parsed default export"
[expect]
exit_code = 0
*/
function fail() {
  return undefined.x;
}
import data from "./_modules/data.json";
if (data.name !== "otter") fail();
if (data.count !== 7) fail();
if (data.tags.length !== 2) fail();
if (data.tags[0] !== "fast") fail();
if (data.tags[1] !== "small") fail();
