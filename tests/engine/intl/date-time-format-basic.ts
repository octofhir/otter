/* otter-test:
name = "intl: DateTimeFormat formats epoch ms to a non-empty string"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
const dtf = new Intl.DateTimeFormat("en-US");
const formatted = dtf.format(1704067200000);
if (formatted === "") fail();
// Expect mm/dd/yyyy-style components (digits + slashes).
if (formatted.length < 5) fail();
const opts = dtf.resolvedOptions();
if (opts.locale !== "en-US") fail();
