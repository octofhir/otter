/* otter-test:
name = "ts: interface erases to nothing"
[expect]
exit_code = 0
*/
interface I {
    x: number;
    y: string;
}
undefined;
