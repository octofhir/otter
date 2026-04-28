/* otter-test:
name = "modules: exported binding mutation is visible to importers"
[expect]
exit_code = 0
*/
import { inc, get } from "./_modules/counter.ts";
function fail() {
    return undefined.x;
}
inc();
inc();
inc();
if (get() !== 3) fail();
