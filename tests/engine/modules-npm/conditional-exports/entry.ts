/* otter-test:
name = "modules-npm: conditional exports map picks the import branch for ESM importers"
[expect]
exit_code = 0
*/
import { kind } from "dual-pkg";
function fail() {
    return undefined.x;
}
if (kind !== "esm") fail();
