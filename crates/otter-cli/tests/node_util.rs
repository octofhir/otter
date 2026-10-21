//! CLI coverage for `node:util`, `util/types`, and `node:tty`.
//!
//! # Contents
//! - `util/types` reference identity with `util.types`.
//! - Dotenv parsing and USV-string normalization.
//! - ANSI named, nested, and hexadecimal style composition.
//! - `node:tty` over pipes and files: stream shapes, color negotiation from
//!   an explicit environment, and the refusals Node raises.
//! - `node:tty` in a pseudo-terminal: standard streams that are terminal
//!   streams, a window size, and raw mode switched on and off and handed
//!   back when the run ends.
//! - `node:tty` and descriptors the run did not open: refused.
//!
//! # Invariants
//! - Hosted aliases share one cached util export per runtime.
//! - A terminal stream over a pipe is a `net.Socket` that answers `isTTY`
//!   from its prototype, has no window size, and colors nothing under
//!   `NO_COLOR`; a regular file is refused with Node's `uv_tty_init` error.
//! - The terminal natives reach the standard streams alone: any other
//!   descriptor, open or not, answers EBADF and receives nothing, and no
//!   terminal stream is made over it. Raw mode leaves nothing on `process`.

use std::process::{Command, Stdio};

#[test]
fn node_util_aliases_and_styles_match_node_shape() {
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        tmp.path().join("main.js"),
        r#"
const util = require('node:util');
if (require('util/types') !== util.types) throw new Error('util/types identity');
const env = util.parseEnv('A=plain # comment\nB="line\\nnext"\nA=last\n');
if (env.A !== 'last' || env.B !== 'line\nnext') throw new Error('parseEnv');
if (util.toUSVString('x\ud801') !== 'x\ufffd') throw new Error('toUSVString');
if (!util.types.isNativeError(new Error()) ||
    util.types.isNativeError({ __proto__: Error.prototype })) {
  throw new Error('isNativeError internal slot');
}
if (util.styleText('#ffcc00', 'x', { validateStream: false }) !==
    '\x1b[38;2;255;204;0mx\x1b[39m') throw new Error('hex style');
if (util.styleText(['bold', 'red'], 'x', { validateStream: false }) !==
    '\x1b[1m\x1b[31mx\x1b[39m\x1b[22m') throw new Error('nested style');
"#,
    )
    .expect("write util fixture");

    let output = Command::new(env!("CARGO_BIN_EXE_otter"))
        .current_dir(tmp.path())
        .arg("run")
        .arg("main.js")
        .output()
        .expect("run util fixture");
    assert_success(&output);
}

#[test]
fn node_tty_over_pipes_and_files_matches_node() {
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        tmp.path().join("main.js"),
        r#"
const util = require('node:util');
const fs = require('node:fs');
const net = require('node:net');
const { Writable } = require('node:stream');
const { isatty, ReadStream, WriteStream } = require('node:tty');

function refusal(make) {
  try { make(); } catch (error) { return error; }
  throw new Error('expected a refusal');
}

// Standard output is the test's pipe: not a terminal, yet a terminal stream
// opens over it, the way libuv's handle opens over any stream.
for (const fd of [1, -1, 1.5, '1', 2 ** 31]) {
  if (isatty(fd) !== false) throw new Error(`isatty(${String(fd)})`);
}
if (process.stdout.isTTY || process.stdout.columns !== undefined) {
  throw new Error('process.stdout over a pipe');
}
const stream = new WriteStream(1);
if (!(stream instanceof net.Socket) || !(stream instanceof Writable)) {
  throw new Error('a WriteStream is a socket');
}
if (stream.isTTY !== true || Object.hasOwn(stream, 'isTTY')) {
  throw new Error('isTTY comes from the prototype');
}
if (stream.columns !== undefined || JSON.stringify(stream.getWindowSize()) !== '[null,null]') {
  throw new Error('a pipe has no window');
}
for (const method of ['cursorTo', 'moveCursor', 'clearLine', 'clearScreenDown', '_refreshSize']) {
  if (typeof stream[method] !== 'function') throw new Error(`missing ${method}`);
}

// NO_COLOR is set for this run; an explicit environment decides alone.
if (util.styleText('red', 'x', { stream }) !== 'x') throw new Error('NO_COLOR');
if (stream.getColorDepth({ TERM: 'xterm-256color' }) !== 8) throw new Error('256 colors');
if (stream.getColorDepth({ TERM: 'dumb' }) !== 1) throw new Error('dumb terminal');
if (stream.getColorDepth({ FORCE_COLOR: '3' }) !== 24) throw new Error('forced colors');
if (!stream.hasColors(256, { TERM: 'xterm-256color' }) || stream.hasColors({ TERM: 'dumb' })) {
  throw new Error('hasColors');
}

const invalid = refusal(() => new WriteStream(-1));
if (invalid.code !== 'ERR_INVALID_FD' ||
    invalid.message !== '"fd" must be a positive integer: -1') {
  throw new Error(`invalid fd: ${invalid.message}`);
}
// A regular file is something no poller can wait on.
const file = fs.openSync('plain.txt', 'w');
for (const Stream of [WriteStream, ReadStream]) {
  const failed = refusal(() => new Stream(file));
  if (failed.code !== 'ERR_TTY_INIT_FAILED' ||
      failed.message !== 'TTY initialization failed: uv_tty_init returned EINVAL (invalid argument)' ||
      failed.info.syscall !== 'uv_tty_init' || failed.info.code !== 'EINVAL' ||
      failed.info.errno !== -22) {
    throw new Error(`file: ${failed.message}`);
  }
}

// Standard input is a pipe here: raw mode is a terminal's alone, and asking
// for it is an 'error' event, not a throw.
const input = new ReadStream(0);
if (input.isTTY !== true || input.isRaw !== false) throw new Error('ReadStream shape');
let rawError;
input.on('error', (error) => { rawError = error; });
input.setRawMode(true);
if (input.isRaw !== false || rawError?.code !== 'ENOTTY' ||
    rawError.syscall !== 'setRawMode' || rawError.message !== 'setRawMode ENOTTY') {
  throw new Error(`raw mode on a pipe: ${rawError?.message}`);
}
input.destroy();
"#,
    )
    .expect("write tty fixture");

    let output = Command::new(env!("CARGO_BIN_EXE_otter"))
        .current_dir(tmp.path())
        .env("NO_COLOR", "1")
        .env_remove("FORCE_COLOR")
        .stdin(Stdio::piped())
        .arg("--allow-read")
        .arg("--allow-write")
        .arg("run")
        .arg("main.js")
        .output()
        .expect("run tty fixture");
    assert_success(&output);
}

/// Standard streams in a pseudo-terminal are terminal streams: input is a
/// `tty.ReadStream` that switches raw mode on and off, and output knows the
/// terminal's size. Switching leaves nothing on `process`, and a program that
/// exits while raw leaves the terminal as it found it. `script` provides the
/// terminal; where it is missing the test has nothing to run in.
#[cfg(unix)]
#[test]
fn node_tty_in_a_pseudo_terminal_is_a_terminal() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let main = tmp.path().join("main.js");
    std::fs::write(
        &main,
        r#"
const tty = require('node:tty');
const checks = [];
checks.push(typeof process.stdin.on === 'function');
checks.push(process.stdin instanceof tty.ReadStream && process.stdin.isTTY === true);
checks.push(tty.isatty(0) && tty.isatty(1));
checks.push(process.stdout.isTTY === true && Number.isInteger(process.stdout.columns));
checks.push(new tty.WriteStream(1).columns === process.stdout.columns);
const keys = Reflect.ownKeys(process).length;
process.stdin.setRawMode(true);
checks.push(process.stdin.isRaw === true);
process.stdin.setRawMode(false);
checks.push(process.stdin.isRaw === false);
// The mode to hand back is the run's to keep: raw mode leaves nothing on
// `process` for a program to read, replace or delete.
checks.push(Reflect.ownKeys(process).length === keys);
console.log(`checks:${checks.join(',')}`);
// Left raw on purpose: the run hands the mode back as it ends.
process.stdin.setRawMode(true);
"#,
    )
    .expect("write terminal fixture");

    // The shell reads the terminal's mode once the program is gone.
    let session = format!(
        "'{}' run '{}'; stty -a",
        env!("CARGO_BIN_EXE_otter"),
        main.display()
    );
    let mut script = Command::new("script");
    if cfg!(target_os = "linux") {
        script.arg("-qec").arg(&session).arg("/dev/null");
    } else {
        script
            .arg("-q")
            .arg("/dev/null")
            .arg("sh")
            .arg("-c")
            .arg(&session);
    }
    let Ok(output) = script.stdin(Stdio::null()).output() else {
        return;
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stdout.contains("checks:true,true,true,true,true,true,true,true"),
        "terminal checks failed\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.split_whitespace().any(|flag| flag == "icanon"),
        "the terminal was left raw\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
}

/// The terminal natives operate on the run's standard streams alone. The
/// shell that launches the run leaves two descriptors open that the run never
/// opened: 8 on its standard error, a pipe, and 9 on a file. No terminal
/// stream is made over either, and a handle pointed at 9 after it was made
/// neither writes to it nor changes its mode or blocking flag: each native
/// answers EBADF, as for a descriptor the process does not have, where the
/// descriptor itself would have taken the write and the flag, and answered
/// ENOTTY or EINVAL for a mode.
#[cfg(unix)]
#[test]
fn node_tty_refuses_descriptors_the_run_did_not_open() {
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        tmp.path().join("main.js"),
        r#"
const fs = require('node:fs');
const tty = require('node:tty');
const util = require('node:util');

function refusal(make) {
  try { make(); } catch (error) { return error; }
  throw new Error('expected a refusal');
}

// Both are open in this process: whatever refuses them is the run's rule.
const open = fs.readdirSync('/dev/fd');
if (!open.includes('8') || !open.includes('9')) {
  throw new Error(`descriptors 8 and 9 are not open: ${open.join(' ')}`);
}

for (const fd of [8, 9]) {
  if (tty.isatty(fd) !== false) throw new Error(`isatty(${fd})`);
  const failed = refusal(() => new tty.WriteStream(fd));
  if (failed.code !== 'ERR_TTY_INIT_FAILED' || failed.info.code !== 'EINVAL') {
    throw new Error(`a stream over ${fd}: ${failed.message}`);
  }
}

// Taking a descriptor over without the constructor: on a live handle, and on
// a bare instance of the class, the way `process.binding('tty_wrap')` and a
// stream's `_handle.constructor` reach it.
const live = new tty.WriteStream(1)._handle;
const bare = Object.create(live.constructor.prototype);
for (const [call, answer] of [
  ['open on a live handle', live.open(9)],
  ['open on a bare handle', live.open.call(bare, 9)],
]) {
  const name = answer < 0 ? util.getSystemErrorName(answer) : String(answer);
  if (name !== 'EINVAL') throw new Error(`${call} answered ${name}`);
}
const { TTY } = process.binding('tty_wrap');
const ctx = {};
new TTY(9, ctx);
if (ctx.code !== 'EINVAL') throw new Error(`the binding's TTY over 9: ${ctx.code}`);

// A handle made over standard output, then pointed elsewhere. It writes
// before it is asked to stop blocking, since a handle that does not block
// writes through the stream table instead.
const handle = new tty.WriteStream(1)._handle;
handle.fd = 9;
const answers = [
  ['write', handle.writeUtf8String({}, 'leaked\n')],
  ['setBlocking', handle.setBlocking(false)],
  ['getWindowSize', handle.getWindowSize([0, 0])],
  ['setRawMode(true)', handle.setRawMode(true)],
];
// Switching back applies the mode a switch to raw read; the descriptor is
// refused before that mode is looked at.
handle._raw = true;
handle._normalMode = new ArrayBuffer(0);
answers.push(['setRawMode(false)', handle.setRawMode(false)]);
for (const [call, answer] of answers) {
  const name = answer < 0 ? util.getSystemErrorName(answer) : String(answer);
  if (name !== 'EBADF') throw new Error(`${call} answered ${name}`);
}
console.log('refused');
"#,
    )
    .expect("write refusal fixture");
    let untouched = tmp.path().join("untouched.txt");
    std::fs::write(&untouched, b"").expect("write untouched file");

    let output = Command::new("sh")
        .current_dir(tmp.path())
        .arg("-c")
        .arg(r#"exec 8>&2 9>>"$1"; exec "$0" --allow-read run main.js"#)
        .arg(env!("CARGO_BIN_EXE_otter"))
        .arg(&untouched)
        .stdin(Stdio::null())
        .output()
        .expect("run refusal fixture");
    assert_success(&output);
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "refused");
    assert_eq!(
        std::fs::read(&untouched).expect("read untouched file"),
        b"",
        "a descriptor the run did not open was written to"
    );
}

fn assert_success(output: &std::process::Output) {
    assert!(
        output.status.success(),
        "otter failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
