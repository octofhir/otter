/* otter-test:
name = "classes: basic class with constructor + method"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
class Animal {
    constructor(n) {
        this.name = n;
    }
    speak() {
        return this.name + " speaks";
    }
}
const a = new Animal("dog");
if (a.speak() !== "dog speaks") fail();
if (a.name !== "dog") fail();
