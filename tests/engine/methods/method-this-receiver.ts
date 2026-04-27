/* otter-test:
name = "methods: this binds to receiver inside obj.method()"
[expect]
exit_code = 0
*/
function fail() {
    // `undefined.x` raises TypeMismatch and surfaces as a non-zero
    // exit code, turning a wrong assertion into a test failure.
    return undefined.x;
}
const greeter = {
    name: "otter",
    speak: function () {
        return this.name;
    },
};
const seen = greeter.speak();
if (seen !== "otter") fail();
