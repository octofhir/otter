/* otter-test:
name = "async generators: await inside body suspends and resumes"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}

async function* g() {
    yield 1;
    const v = await Promise.resolve(2);
    yield v;
    const w = await Promise.resolve(3);
    yield w + 10;
}

async function main() {
    const out = [];
    for await (const v of g()) {
        out.push(v);
    }
    if (out.length !== 3) fail();
    if (out[0] !== 1) fail();
    if (out[1] !== 2) fail();
    if (out[2] !== 13) fail();
}

main();
