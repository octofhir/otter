/* otter-test:
name = "intl: Collator.compare orders strings per locale"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
const c = new Intl.Collator("en");
if (c.compare("a", "b") >= 0) fail();
if (c.compare("b", "a") <= 0) fail();
if (c.compare("x", "x") !== 0) fail();
const opts = c.resolvedOptions();
if (opts.locale !== "en") fail();
