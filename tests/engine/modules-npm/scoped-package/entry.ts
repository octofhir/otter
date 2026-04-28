/* otter-test:
name = "modules-npm: scoped @scope/pkg resolves through node_modules"
[expect]
exit_code = 0
*/
import { value, squared } from "@scope/ns";
function fail() {
    return undefined.x;
}
if (value !== 9) fail();
if (squared() !== 81) fail();
