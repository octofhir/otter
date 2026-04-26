/* otter-test:
name = "ts: namespace with runtime body is rejected"
[expect]
exit_code = 1
*/
namespace Runtime {
    export const x = 1;
}
