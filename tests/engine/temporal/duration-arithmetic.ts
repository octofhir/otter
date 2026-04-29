/* otter-test:
name = "temporal: Duration add/subtract/negated/abs"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
const a = Temporal.Duration.from({ hours: 1 });
const b = Temporal.Duration.from({ minutes: 30 });
const sum = a.add(b);
if (sum.total({ unit: "minutes" }) !== 90) fail();
const diff = a.subtract(b);
if (diff.total({ unit: "minutes" }) !== 30) fail();
const neg = a.negated();
if (neg.total({ unit: "minutes" }) !== -60) fail();
const back = neg.abs();
if (back.total({ unit: "minutes" }) !== 60) fail();
