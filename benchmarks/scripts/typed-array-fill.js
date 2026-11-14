// Setup is outside measured samples; alternating values reject stale stores.
const typedFillInput = new Uint8Array(8388611);
let typedFillValue = 19;

function engineKernel() {
    typedFillValue = typedFillValue === 7 ? 19 : 7;
    typedFillInput.fill(typedFillValue);
    return typedFillInput.length + typedFillInput[0] + typedFillInput[8388610]
        - 2 * typedFillValue + 14;
}
