/* otter-test:
name = "classes: derived class with user ctor runs fields after super()"
[expect]
exit_code = 0
*/
function fail() {
  return undefined.x;
}
class Animal {
  legs = 4;
  constructor() {}
}
class Dog extends Animal {
  name = "rex";
  age: number;
  constructor(age: number) {
    super();
    // Field initializers ran after super() returned.
    if (this.legs !== 4) fail();
    if (this.name !== "rex") fail();
    this.age = age;
  }
}
let d = new Dog(5);
if (d.legs !== 4) fail();
if (d.name !== "rex") fail();
if (d.age !== 5) fail();
