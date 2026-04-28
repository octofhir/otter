/* otter-test:
name = "modules-npm: bare specifier resolves through local node_modules"
[expect]
exit_code = 0
*/
import { answer, double } from "util-pkg";
function fail() {
    return undefined.x;
}
if (answer !== 42) fail();
if (double(21) !== 42) fail();
