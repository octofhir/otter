/* otter-test:
name = "modules-npm: bare specifier walks up node_modules from a deep directory"
[expect]
exit_code = 0
*/
import { identifier } from "shared-util";
function fail() {
    return undefined.x;
}
if (identifier !== "ancestor-shared-util") fail();
