// Concat feedback from a prior call must not convert a later numeric Add.
function labelAfterConcat(x, suffix) {
    return x + 1 + (suffix === undefined ? 0 : suffix);
}
let beforeConcat = 0;
for (let index = 0; index < 40000; index++) beforeConcat += labelAfterConcat(index);
const mixedConcatCalls = [
    labelAfterConcat(1, "!"),
    labelAfterConcat(2.5),
    labelAfterConcat(3, 4)
];
let afterConcat = 0;
for (let index = 0; index < 40000; index++) afterConcat += labelAfterConcat(index, 1);
JSON.stringify([
    beforeConcat,
    mixedConcatCalls,
    typeof afterConcat,
    afterConcat === 800060000
]);
