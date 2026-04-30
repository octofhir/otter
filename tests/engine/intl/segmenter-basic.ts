/* otter-test:
name = "intl: Segmenter grapheme / word / sentence segmentation"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}

// Default granularity is grapheme — one segment per code point.
const seg = new Intl.Segmenter("en");
const graphemes = seg.segment("abc");
if (graphemes.length !== 3) fail();
if (graphemes[0].segment !== "a") fail();
if (graphemes[2].segment !== "c") fail();
if (graphemes[1].index !== 1) fail();
if (graphemes[0].input !== "abc") fail();

// Word granularity exposes `isWordLike`.
const wseg = new Intl.Segmenter("en", { granularity: "word" });
const words = wseg.segment("hello world");
let found = 0;
let i = 0;
while (i < words.length) {
    if (words[i].isWordLike === true) found = found + 1;
    i = i + 1;
}
if (found < 2) fail();

// Sentence granularity splits on . / ! / ?.
const sseg = new Intl.Segmenter("en", { granularity: "sentence" });
const sentences = sseg.segment("Hi. Bye!");
if (sentences.length < 2) fail();

const opts = seg.resolvedOptions();
if (opts.granularity !== "grapheme") fail();
