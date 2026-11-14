// Reverse twice so each invocation restores the same non-uniform input.
const reverseCount = 8388608;
const reverseInput = new Uint8Array(reverseCount);
reverseInput[0] = 7;
reverseInput[reverseCount - 1] = 19;

function engineKernel() {
    reverseInput.reverse();
    const first = reverseInput[0] === 19 && reverseInput[reverseCount - 1] === 7;
    reverseInput.reverse();
    return reverseCount + (first ? 7 : 0)
        + (reverseInput[0] === 7 && reverseInput[reverseCount - 1] === 19 ? 7 : 0);
}
