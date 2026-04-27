/* otter-test:
name = "classes: methods bind `this`, instances iterate via for-of"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
class Bag {
    constructor() {
        this.items = [];
    }
    push(v) {
        this.items.push(v);
        return this;
    }
    sum() {
        let total = 0;
        for (let n of this.items) {
            total = total + n;
        }
        return total;
    }
}
const b = new Bag();
b.push(1).push(2).push(3);
if (b.sum() !== 6) fail();
