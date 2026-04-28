/* otter-test:
name = "modules: namespace import binds the module_env directly"
[expect]
exit_code = 0
*/
import * as api from "./_modules/api.ts";
function fail() {
    return undefined.x;
}
if (api.one !== 1) fail();
if (api.two !== 2) fail();
if (api.sum() !== 3) fail();
