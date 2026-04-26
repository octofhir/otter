/* otter-test:
name = "control-flow: for loop sums 0..9"
[expect]
exit_code = 0
*/
let sum = 0;
for (let i = 0; i < 10; i = i + 1) {
    sum = sum + i;
}
sum;
