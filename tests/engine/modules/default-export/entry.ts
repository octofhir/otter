/* otter-test:
name = "modules: default export imports under chosen alias"
[expect]
exit_code = 0
*/
import greet from "./_modules/greeter.ts";
function fail() {
    return undefined.x;
}
if (greet("world") !== "hello world") fail();
