/* otter-test:
name = "temporal: Temporal.Now views return live values"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
const a = Temporal.Now.instant();
const b = Temporal.Now.instant();
if (a.epochMilliseconds < 1700000000000) fail();
if (b.epochMilliseconds < a.epochMilliseconds) fail();
const dt = Temporal.Now.plainDateTimeISO();
if (dt.year < 2024) fail();
const date = Temporal.Now.plainDateISO();
if (date.year < 2024) fail();
const time = Temporal.Now.plainTimeISO();
if (time.hour < 0) fail();
if (time.hour > 23) fail();
