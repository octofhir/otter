/* otter-test:
name = "modules: static import of a named export reads it"
[expect]
exit_code = 0
*/
import { value } from "./_modules/other.ts";
function fail() {
    return undefined.x;
}
if (value !== 7) fail();
