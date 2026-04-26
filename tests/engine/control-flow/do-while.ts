/* otter-test:
name = "control-flow: do/while runs at least once"
[expect]
exit_code = 0
*/
let n = 0;
do {
    n = n + 1;
} while (n < 3);
n;
