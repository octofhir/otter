/* otter-test:
name = "modules: literal import() resolves to namespace via promise"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
import("./_modules/util.ts").then((m) => {
    if (m.answer !== 42) fail();
    if (m.double(21) !== 42) fail();
});
