// Test: clearInterval should work inside callback
let count = 0;
const id = setInterval(() => {
    count++;
    console.log(`tick ${count}`);
    if (count >= 3) {
        clearInterval(id);
        console.log("PASS: clearInterval worked inside callback");
    }
}, 50);

// Expected output:
// tick 1
// tick 2
// tick 3
// PASS: clearInterval worked inside callback
// (program exits)
