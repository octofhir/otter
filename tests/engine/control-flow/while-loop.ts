/* otter-test:
name = "control-flow: while loop sums 1..5"
[expect]
exit_code = 0
*/
let i = 0;
let sum = 0;
while (i < 5) {
    sum = sum + i;
    i = i + 1;
}
sum;
