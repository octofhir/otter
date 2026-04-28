/* otter-test:
name = "modules: diamond graph evaluates each leaf exactly once"
[expect]
exit_code = 0
*/
import { leftTicks } from "./_modules/left.ts";
import { rightTicks } from "./_modules/right.ts";
function fail() {
    return undefined.x;
}
// leaf.ts ran exactly once on graph init, so counter == 1.
// left.ts and right.ts share the same module record.
if (leftTicks() !== 1) fail();
if (rightTicks() !== 1) fail();
