/* otter-test:
name = "temporal: PlainDateTime.from + components"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
const dt = Temporal.PlainDateTime.from("2024-06-15T08:30:00");
if (dt.year !== 2024) fail();
if (dt.month !== 6) fail();
if (dt.day !== 15) fail();
if (dt.hour !== 8) fail();
if (dt.minute !== 30) fail();
const later = dt.add({ days: 1, hours: 2 });
if (later.day !== 16) fail();
if (later.hour !== 10) fail();
