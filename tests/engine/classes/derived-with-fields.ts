/* otter-test:
name = "classes: derived class with synthetic ctor inherits + initializes own fields"
[expect]
exit_code = 0
*/
function fail() {
  return undefined.x;
}
class Animal {
  legs = 4;
  static count = 0;
}
class Dog extends Animal {
  // No user constructor — synthesised derived ctor calls super()
  // then runs the field initializers.
  name = "rex";
  bark() {
    return this.name + "/" + this.legs;
  }
}
let d = new Dog();
if (d.legs !== 4) fail();
if (d.name !== "rex") fail();
if (d.bark() !== "rex/4") fail();
// Static side inherits via prototype chain.
if (Dog.count !== 0) fail();
