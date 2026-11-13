// FindViaPredicate must walk lazily, including absent indices at large lengths.
function checkLargeFind(condition, message) {
    if (!condition) throw new Error(message);
}
for (const length of [2 ** 26, Number.MAX_SAFE_INTEGER]) {
    for (const name of ["find", "findIndex", "findLast", "findLastIndex"]) {
        const reverse = name === "findLast" || name === "findLastIndex";
        const first = reverse ? length - 1 : 0;
        const second = reverse ? length - 2 : 1;
        const value = {text: "\ud800find\udc00"};
        const thisArg = {};
        const reads = [];
        let lengthReads = 0, calls = 0;
        const receiver = new Proxy({length}, {
            get(target, key) {
                if (key === "length") { lengthReads++; return target.length; }
                reads.push(key);
                return Reflect.get(target, key);
            },
            has() { throw new Error("unexpected HasProperty"); },
            ownKeys() { throw new Error("unexpected enumeration"); },
            getPrototypeOf() { throw new Error("unexpected prototype walk"); }
        });
        const result = Array.prototype[name].call(receiver, function(item, index, object) {
            checkLargeFind(this === thisArg && arguments.length === 3 && object === receiver,
                "callback receiver and arguments");
            calls++;
            if (calls === 1) {
                checkLargeFind(index === first && item === undefined, "first absent index");
                object.length = 0;
                object[second] = value;
                for (let n = 0; n < 16; n++) globalThis.findRangeChurn = {n, child: {n}};
                return false;
            }
            checkLargeFind(calls === 2 && index === second && item === value,
                "next live indexed Get after mutation");
            return true;
        }, thisArg);
        checkLargeFind(result === (name.endsWith("Index") ? second : value), "early result");
        checkLargeFind(calls === 2 && lengthReads === 1 && reads.join(",") === first + "," + second,
            "exact order and snapshotted length");
        checkLargeFind(value.text.charCodeAt(0) === 0xd800 && value.text.charCodeAt(5) === 0xdc00,
            "exact UTF-16 payload");
    }
}
// The same driver must re-read a moving Array receiver and a live own element.
for (const name of ["find", "findIndex", "findLast", "findLastIndex"]) {
    const reverse = name === "findLast" || name === "findLastIndex";
    const value = {marker: 73};
    const receiver = reverse ? [value, undefined] : [undefined, value];
    let calls = 0;
    const result = receiver[name]((item) => {
        if (++calls === 1) {
            for (let n = 0; n < 16; n++) globalThis.findRangeChurn = {n, child: {n}};
            return false;
        }
        checkLargeFind(item === value && item.marker === 73, "live Array element after allocation");
        return true;
    });
    checkLargeFind(calls === 2 && result === (name.endsWith("Index") ? (reverse ? 0 : 1) : value),
        "Array receiver and returned element survive callbacks");
}
for (const name of ["find", "findIndex", "findLast", "findLastIndex"]) {
    const marker = {};
    let called = false, caught;
    const receiver = new Proxy({length: Number.MAX_SAFE_INTEGER}, {
        get(target, key) { if (key === "length") return target.length; throw marker; },
        has() { throw new Error("unexpected HasProperty before abrupt Get"); },
        ownKeys() { throw new Error("unexpected enumeration before abrupt Get"); },
        getPrototypeOf() { throw new Error("unexpected prototype walk before abrupt Get"); }
    });
    try { Array.prototype[name].call(receiver, () => { called = true; return false; }); }
    catch (error) { caught = error; }
    checkLargeFind(caught === marker && !called, "Get abrupt completion precedes callback");
    caught = undefined;
    try { Array.prototype[name].call({get length() { throw marker; }}, null); }
    catch (error) { caught = error; }
    checkLargeFind(caught === marker, "length abrupt completion precedes callback validation");
}
console.log("ok:large-find-range");
"ok:large-find-range";
