/* otter-test:
name = "classes: static fields + static blocks run at declaration time"
[expect]
exit_code = 0
*/
function fail() {
  return undefined.x;
}
class C {
  static a = 1;
  static b = C.a + 10;
  static {
    // `this` inside a static block refers to the class statics.
    this.b = this.b + 100;
  }
  static c = "set after blocks";
  static {
    this.tag = this.c.length;
  }
}
if (C.a !== 1) fail();
if (C.b !== 111) fail();
if (C.c !== "set after blocks") fail();
if ((C as any).tag !== "set after blocks".length) fail();

// Static block with locals + control flow.
class Init {
  static counter = 0;
  static {
    let total = 0;
    for (let i = 1; i <= 5; i = i + 1) {
      total = total + i;
    }
    this.counter = total;
  }
}
if (Init.counter !== 15) fail();
