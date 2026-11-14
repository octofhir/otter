// Two overlapping 8 MiB moves, with changing boundary bytes each invocation.
const copyBlock = 8388608;
const copyBytes = new Uint8Array(copyBlock + 2);
let copyValue = 19;

function engineKernel() {
    copyValue = copyValue === 7 ? 19 : 7;
    copyBytes[0] = copyValue;
    copyBytes[copyBlock - 1] = copyValue + 1;
    copyBytes.copyWithin(1, 0, copyBlock);
    const right = copyBytes[1] === copyValue;
    copyBytes.copyWithin(0, 1, copyBlock + 1);
    return copyBlock + (right ? 4 : 0) + (copyBytes[0] === copyValue ? 4 : 0)
        + (copyBytes[copyBlock - 1] === copyValue + 1 ? 6 : 0);
}
