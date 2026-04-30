/* otter-test:
name = "intl: RelativeTimeFormat formats English templates"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}

const rtf = new Intl.RelativeTimeFormat("en");
if (rtf.format(3, "day") !== "in 3 days") fail();
if (rtf.format(1, "hour") !== "in 1 hour") fail();
if (rtf.format(-1, "minute") !== "1 minute ago") fail();
if (rtf.format(-5, "month") !== "5 months ago") fail();

// formatToParts shape: returns an array with at least one part.
const parts = rtf.formatToParts(2, "year");
if (parts.length < 1) fail();
if (parts[0].type !== "literal") fail();

const opts = rtf.resolvedOptions();
if (opts.style !== "long") fail();
if (opts.numeric !== "always") fail();
