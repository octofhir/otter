/* otter-test:
name = "control-flow: break + continue"
[expect]
exit_code = 0
*/
let total = 0;
let i = 0;
while (i < 10) {
    i = i + 1;
    if (i === 3) {
        continue;
    }
    if (i === 7) {
        break;
    }
    total = total + i;
}
total;
