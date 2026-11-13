// Borrow Array fill over a fixed numeric view beyond the old span ceiling.
// Setup stays outside measured samples. Alternating values make a stale fill
// fail the next invocation's checksum; every call must reach the last element.
const fillNumericInput = new Uint8Array(33554433);
let fillNumericValue = 19;

function engineKernel() {
    fillNumericValue = fillNumericValue === 7 ? 19 : 7;
    Array.prototype.fill.call(fillNumericInput, fillNumericValue);
    return fillNumericInput.length + fillNumericInput[0] + fillNumericInput[33554432]
        - 2 * fillNumericValue + 14;
}
