/* otter-test:
name = "intl: NumberFormat decimal style with grouping"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
const nf = new Intl.NumberFormat("en-US");
if (nf.format(1234567) !== "1,234,567") fail();
const noGroup = new Intl.NumberFormat("en-US", { useGrouping: false });
if (noGroup.format(1234567) !== "1234567") fail();
