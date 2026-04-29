/* otter-test:
name = "temporal: Duration.from + total({ unit })"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
const d = Temporal.Duration.from({ hours: 1, minutes: 30 });
if (d.hours !== 1) fail();
if (d.minutes !== 30) fail();
const totalMin = d.total({ unit: "minutes" });
if (totalMin !== 90) fail();
const totalSec = d.total({ unit: "seconds" });
if (totalSec !== 5400) fail();
