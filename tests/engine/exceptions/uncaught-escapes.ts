/* otter-test:
name = "exceptions: uncaught throw escapes as non-zero exit"
[expect]
exit_code = 1
*/
throw new Error("unhandled");
