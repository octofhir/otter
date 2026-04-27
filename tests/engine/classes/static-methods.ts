/* otter-test:
name = "classes: static methods on the constructor + extends"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
class Counter {
    static greet() {
        return "hi";
    }
}
class Loud extends Counter {
    static shout() {
        return Counter.greet() + "!";
    }
}
if (Counter.greet() !== "hi") fail();
// `extends` chains the static side too — `Loud.greet` resolves up.
if (Loud.greet() !== "hi") fail();
if (Loud.shout() !== "hi!") fail();
