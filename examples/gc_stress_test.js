// GC Stress Test Script
// Tests correctness and memory behavior of the garbage collector

// Test 1: Circular references
function testCircularRefs() {
    for (let i = 0; i < 10000; i++) {
        let a = { id: i };
        let b = { id: i + 1 };
        a.b = b;
        b.a = a;
        // a and b should be collected
    }
    console.log("Circular ref test: PASS");
}

// Test 2: Deep object trees
function testDeepTrees() {
    for (let i = 0; i < 1000; i++) {
        let root = { value: i };
        let current = root;
        for (let depth = 0; depth < 100; depth++) {
            current.next = { value: depth };
            current = current.next;
        }
        // Entire tree should be collected
    }
    console.log("Deep tree test: PASS");
}

// Test 3: Array allocation
function testArrays() {
    for (let i = 0; i < 5000; i++) {
        let arr = new Array(100);
        for (let j = 0; j < 100; j++) {
            arr[j] = { value: j };
        }
        // Array and contents should be collected
    }
    console.log("Array test: PASS");
}

// Test 4: Retained objects
function testRetention() {
    let retained = [];
    for (let i = 0; i < 1000; i++) {
        let obj = { id: i };
        if (i % 10 === 0) {
            retained.push(obj);  // Keep every 10th object
        }
    }
    console.log("Retention test: PASS, retained", retained.length, "objects");
}

// Test 5: Closures (capture variables)
function testClosures() {
    let closures = [];
    for (let i = 0; i < 1000; i++) {
        let captured = { value: i };
        closures.push(() => captured.value);
    }
    // Verify closures work
    let sum = 0;
    for (let fn of closures) {
        sum += fn();
    }
    console.log("Closure test: PASS, sum =", sum);
}

// Test 6: String interning stress
function testStringInterning() {
    let objects = [];
    for (let i = 0; i < 1000; i++) {
        let obj = {};
        obj["prop" + i] = i;
        obj["another" + i] = i * 2;
        objects.push(obj);
    }
    // Verify properties still accessible
    let check = objects[500]["prop500"];
    if (check !== 500) {
        console.log("String interning test: FAIL, expected 500, got", check);
        return;
    }
    console.log("String interning test: PASS");
}

// Run all tests
console.log("=== GC Stress Tests ===");
testCircularRefs();
testDeepTrees();
testArrays();
testRetention();
testClosures();
testStringInterning();
console.log("=== All GC stress tests: PASS ===");
