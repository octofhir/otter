/* otter-test:
name = "classes: extends + super forwards constructor and methods"
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
    describe() {
        return "animal:" + this.name;
    }
}
class Dog extends Animal {
    constructor(n, breed) {
        super(n);
        this.breed = breed;
    }
    describe() {
        return super.describe() + ":" + this.breed;
    }
}
const d = new Dog("rex", "husky");
if (d.name !== "rex") fail();
if (d.breed !== "husky") fail();
if (d.describe() !== "animal:rex:husky") fail();
