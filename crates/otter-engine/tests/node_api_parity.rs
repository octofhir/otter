use otter_engine::{CapabilitiesBuilder, EngineBuilder, NodeApiProfile, Otter};
use tempfile::tempdir;

fn js_string(input: &str) -> String {
    serde_json::to_string(input).unwrap()
}

fn full_engine_for_parity() -> Otter {
    let caps = CapabilitiesBuilder::new()
        .allow_read_all()
        .allow_write_all()
        .allow_env_all()
        .allow_hrtime()
        .allow_subprocess()
        .build();

    EngineBuilder::new()
        .with_nodejs_profile(NodeApiProfile::Full)
        .capabilities(caps)
        .build()
}

fn assert_ok(otter: &mut Otter, code: &str) {
    let value = otter
        .eval_sync(code)
        .unwrap_or_else(|e| panic!("Eval failed: {e}"));
    let out = value.as_string().map(|s| s.to_string()).unwrap_or_default();
    assert_eq!(out, "ok");
}

#[test]
fn test_fs_extended_sync_api_workflow() {
    let dir = tempdir().unwrap();
    let mut otter = full_engine_for_parity();
    let root = js_string(&dir.path().to_string_lossy());
    let code = format!(
        "import fs from 'node:fs'; import path from 'node:path';\n\
         const root = {root};\n\
         const a = path.join(root, 'a.txt');\n\
         const b = path.join(root, 'b.txt');\n\
         const c = path.join(root, 'c.txt');\n\
         fs.writeFileSync(a, 'A');\n\
         fs.appendFileSync(a, 'B');\n\
         fs.copyFileSync(a, b);\n\
         fs.renameSync(b, c);\n\
         fs.accessSync(c, fs.constants.R_OK);\n\
         if (fs.readFileSync(a, 'utf8') !== 'AB') throw new Error('append failed');\n\
         if (fs.readFileSync(c, 'utf8') !== 'AB') throw new Error('copy/rename failed');\n\
         const rp = fs.realpathSync(c);\n\
         if (typeof rp !== 'string' || rp.length === 0) throw new Error('realpath');\n\
         const tmp = fs.mkdtempSync(path.join(root, 'tmp-'));\n\
         if (!fs.existsSync(tmp)) throw new Error('mkdtemp');\n\
         fs.rmSync(c);\n\
         fs.rmSync(tmp, {{ recursive: true, force: true }});\n\
         if (fs.existsSync(c)) throw new Error('rm');\n\
         'ok';"
    );
    assert_ok(&mut otter, &code);
}

#[test]
fn test_fs_promises_extended_api_surface() {
    let dir = tempdir().unwrap();
    let mut otter = full_engine_for_parity();
    let root = js_string(&dir.path().to_string_lossy());
    let code = format!(
        "import fs from 'node:fs'; import fsp from 'node:fs/promises'; import path from 'node:path';\n\
         const p = path.join({root}, 'promises.txt');\n\
         fs.writeFileSync(p, 'x');\n\
         const pending = fsp.readFile(p, 'utf8');\n\
         if (typeof pending?.then !== 'function') throw new Error('readFile promise');\n\
         const openPending = fsp.open(p, 'r');\n\
         if (typeof openPending?.then !== 'function') throw new Error('open promise');\n\
         for (const name of ['appendFile','mkdtemp','cp','open','opendir','rm','access','copyFile','rename','realpath']) {{\n\
             if (typeof fsp[name] !== 'function') throw new Error('missing ' + name);\n\
         }}\n\
         'ok';"
    );
    assert_ok(&mut otter, &code);
}

#[test]
fn test_fs_readdir_with_file_types_and_promises_bridge() {
    let dir = tempdir().unwrap();
    let mut otter = full_engine_for_parity();
    let root = js_string(&dir.path().to_string_lossy());
    let code = format!(
        "import fs from 'node:fs'; import path from 'node:path';\n\
         const root = {root};\n\
         fs.mkdirSync(path.join(root, 'nested'));\n\
         fs.writeFileSync(path.join(root, 'a.txt'), 'x');\n\
         const entries = fs.readdirSync(root, {{ withFileTypes: true }});\n\
         if (!Array.isArray(entries) || entries.length < 2) throw new Error('dirent list');\n\
         const fileEnt = entries.find((e) => e.name === 'a.txt');\n\
         const dirEnt = entries.find((e) => e.name === 'nested');\n\
         if (!fileEnt || typeof fileEnt.isFile !== 'function' || !fileEnt.isFile()) throw new Error('dirent file');\n\
         if (!dirEnt || typeof dirEnt.isDirectory !== 'function' || !dirEnt.isDirectory()) throw new Error('dirent dir');\n\
         if (typeof fs.promises !== 'object' || typeof fs.promises.open !== 'function') throw new Error('fs.promises bridge');\n\
         'ok';"
    );
    assert_ok(&mut otter, &code);
}

#[test]
fn test_fs_callback_async_api_surface() {
    let dir = tempdir().unwrap();
    let mut otter = full_engine_for_parity();
    let root = js_string(&dir.path().to_string_lossy());
    let code = format!(
        "import fs from 'node:fs'; import path from 'node:path';\n\
         const p = path.join({root}, 'cb.txt');\n\
         fs.writeFileSync(p, 'cb');\n\
         const cb = () => {{}};\n\
         for (const name of ['readFile','writeFile','appendFile','stat','lstat','readdir','mkdir','mkdtemp','rm','unlink','cp','copyFile','rename','realpath','access','chmod','symlink','readlink','open','opendir']) {{\n\
             if (typeof fs[name] !== 'function') throw new Error('missing callback api ' + name);\n\
         }}\n\
         if (fs.readFile(p, 'utf8', cb) !== undefined) throw new Error('readFile callback return');\n\
         if (fs.writeFile(p, 'x', cb) !== undefined) throw new Error('writeFile callback return');\n\
         if (fs.readdir({root}, cb) !== undefined) throw new Error('readdir callback return');\n\
         if (fs.open(p, 'r', cb) !== undefined) throw new Error('open callback return');\n\
         if (fs.opendir({root}, cb) !== undefined) throw new Error('opendir callback return');\n\
         'ok';"
    );
    assert_ok(&mut otter, &code);
}

#[test]
fn test_fs_cp_options_and_symlink_behavior() {
    let dir = tempdir().unwrap();
    let mut otter = full_engine_for_parity();
    let root = js_string(&dir.path().to_string_lossy());
    let code = format!(
        "import fs from 'node:fs'; import path from 'node:path';\n\
         const root = {root};\n\
         const src = path.join(root, 'src.txt');\n\
         const dst = path.join(root, 'dst.txt');\n\
         fs.writeFileSync(src, 'source');\n\
         fs.writeFileSync(dst, 'existing');\n\
         fs.cpSync(src, dst, {{ force: false }});\n\
         if (fs.readFileSync(dst, 'utf8') !== 'existing') throw new Error('cp force false');\n\
         let threw = false;\n\
         try {{ fs.cpSync(src, dst, {{ force: false, errorOnExist: true }}); }} catch (_e) {{ threw = true; }}\n\
         if (!threw) throw new Error('cp errorOnExist');\n\
         fs.cpSync(src, dst);\n\
         if (fs.readFileSync(dst, 'utf8') !== 'source') throw new Error('cp default force');\n\
         const target = path.join(root, 'target.txt');\n\
         const linkSrc = path.join(root, 'link-src');\n\
         const linkDst = path.join(root, 'link-dst');\n\
         const derefDst = path.join(root, 'deref.txt');\n\
         fs.writeFileSync(target, 'linkdata');\n\
         let symlinkOk = true;\n\
         try {{ fs.symlinkSync(target, linkSrc); }} catch (_e) {{ symlinkOk = false; }}\n\
         if (symlinkOk) {{\n\
             fs.cpSync(linkSrc, linkDst, {{ dereference: false, force: true }});\n\
             if (!fs.lstatSync(linkDst).isSymbolicLink()) throw new Error('cp symlink preserve');\n\
             fs.cpSync(linkSrc, derefDst, {{ dereference: true, force: true }});\n\
             if (fs.lstatSync(derefDst).isSymbolicLink()) throw new Error('cp dereference type');\n\
             if (fs.readFileSync(derefDst, 'utf8') !== 'linkdata') throw new Error('cp dereference content');\n\
         }}\n\
         const treeSrc = path.join(root, 'tree-src');\n\
         const treeDst = path.join(root, 'tree-dst');\n\
         fs.mkdirSync(treeSrc, {{ recursive: true }});\n\
         fs.writeFileSync(path.join(treeSrc, 'keep.txt'), 'k');\n\
         fs.writeFileSync(path.join(treeSrc, 'skip.txt'), 's');\n\
         fs.cpSync(treeSrc, treeDst, {{ recursive: true, filter: (srcPath, _dstPath) => !String(srcPath).endsWith('skip.txt') }});\n\
         if (!fs.existsSync(path.join(treeDst, 'keep.txt'))) throw new Error('cp filter keep');\n\
         if (fs.existsSync(path.join(treeDst, 'skip.txt'))) throw new Error('cp filter skip');\n\
         let modeThrew = false;\n\
         try {{ fs.cpSync(src, dst, {{ mode: 1 }}); }} catch (_e) {{ modeThrew = true; }}\n\
         if (!modeThrew) throw new Error('cp mode excl');\n\
         'ok';"
    );
    assert_ok(&mut otter, &code);
}

#[test]
fn test_fs_opendir_sync_and_abort_signal_surface() {
    let dir = tempdir().unwrap();
    let mut otter = full_engine_for_parity();
    let root = dir.path().to_string_lossy().to_string();
    let root_js = js_string(&root);

    let code = format!(
        "import fs from 'node:fs'; import fsp from 'node:fs/promises'; import path from 'node:path';\n\
         const root = {root_js};\n\
         fs.writeFileSync(path.join(root, 'a.txt'), 'a');\n\
         fs.writeFileSync(path.join(root, 'b.txt'), 'b');\n\
         const dirh = fs.opendirSync(root);\n\
         const names = [];\n\
         for (;;) {{\n\
             const ent = dirh.readSync();\n\
             if (ent === null) break;\n\
             names.push(ent.name);\n\
         }}\n\
         dirh.closeSync();\n\
         names.sort();\n\
         if (names.join(',') !== 'a.txt,b.txt') throw new Error('opendir async iterator');\n\
         const pending = fsp.readFile(path.join(root, 'a.txt'), {{ signal: {{ aborted: true }} }});\n\
         if (typeof pending?.then !== 'function') throw new Error('abort promise surface');\n\
         'ok';"
    );

    assert_ok(&mut otter, &code);
}

#[tokio::test]
async fn test_eval_waits_for_detached_fs_async_ops() {
    let dir = tempdir().unwrap();
    let mut otter = full_engine_for_parity();
    let file = dir.path().join("detached.txt");
    let file_js = js_string(&file.to_string_lossy());

    let code = format!(
        "import fs from 'node:fs'; import fsp from 'node:fs/promises';\n\
         const p = {file_js};\n\
         fs.writeFileSync(p, 'seed');\n\
         fsp.writeFile(p, 'promises-done');\n\
         'ok';"
    );

    let _value = otter.eval(&code, None).await.expect("eval");

    let written = std::fs::read_to_string(&file).expect("read file");
    assert_eq!(written, "promises-done");
}

#[test]
fn test_process_extended_api_surface() {
    let mut otter = full_engine_for_parity();
    assert_ok(
        &mut otter,
        "import process from 'node:process';\n\
         if (typeof process.uptime() !== 'number') throw new Error('uptime');\n\
         const mem = process.memoryUsage();\n\
         for (const k of ['rss','heapTotal','heapUsed','external','arrayBuffers']) {\n\
             if (typeof mem[k] !== 'number') throw new Error('memoryUsage ' + k);\n\
         }\n\
         if (typeof process.execPath !== 'string') throw new Error('execPath');\n\
         if (typeof process.argv0 !== 'string') throw new Error('argv0');\n\
         if (typeof process.hrtime.bigint !== 'function') throw new Error('hrtime.bigint');\n\
         process.exitCode = 9;\n\
         if (process.exitCode !== 9) throw new Error('exitCode');\n\
         'ok';",
    );
}

#[test]
fn test_util_extended_api_surface() {
    let mut otter = full_engine_for_parity();
    assert_ok(
        &mut otter,
        "import util from 'node:util';\n\
         if (!util.types.isArray([])) throw new Error('isArray');\n\
         if (!util.types.isDate(new Date())) throw new Error('isDate');\n\
         if (!util.types.isMap(new Map())) throw new Error('isMap');\n\
         if (!util.isPrimitive(1) || util.isPrimitive({})) throw new Error('isPrimitive');\n\
         if (!util.isDeepStrictEqual({ a: 1 }, { a: 1 })) throw new Error('isDeepStrictEqual');\n\
         const fmt = util.format('%s:%d', 'x', 7);\n\
         if (fmt !== 'x:7') throw new Error('format:' + String(fmt));\n\
         if (typeof util.stripVTControlCharacters('\\u001b[31mred\\u001b[0m') !== 'string') throw new Error('stripVT');\n\
         'ok';",
    );
}

#[test]
fn test_os_extended_api_surface() {
    let mut otter = full_engine_for_parity();
    assert_ok(
        &mut otter,
        "import os from 'node:os';\n\
         if (os.endianness() !== 'LE') throw new Error('endianness');\n\
         if (typeof os.availableParallelism() !== 'number') throw new Error('parallelism');\n\
         if (!Array.isArray(os.loadavg()) || os.loadavg().length !== 3) throw new Error('loadavg');\n\
         if (!Array.isArray(os.cpus())) throw new Error('cpus');\n\
         if (typeof os.machine() !== 'string') throw new Error('machine');\n\
         if (typeof os.devNull !== 'string') throw new Error('devNull');\n\
         'ok';",
    );
}

#[test]
fn test_assert_extended_api_surface() {
    let mut otter = full_engine_for_parity();
    assert_ok(
        &mut otter,
        "import assert from 'node:assert';\n\
         assert.match('hello', /ell/);\n\
         assert.doesNotMatch('hello', /xyz/);\n\
         assert.ifError(null);\n\
         if (typeof assert.rejects !== 'function') throw new Error('rejects');\n\
         if (typeof assert.strict !== 'object') throw new Error('strict');\n\
         'ok';",
    );
}

#[test]
fn test_assert_strict_module() {
    let mut otter = full_engine_for_parity();
    assert_ok(
        &mut otter,
        "import assertStrict from 'node:assert/strict';\n\
         assertStrict.equal(1, 1);\n\
         if (typeof assertStrict.match !== 'function') throw new Error('match');\n\
         'ok';",
    );
}

#[test]
fn test_events_extended_api_surface() {
    let mut otter = full_engine_for_parity();
    assert_ok(
        &mut otter,
        "import { EventEmitter, once, listenerCount, setMaxListeners } from 'node:events';\n\
         const ee = new EventEmitter();\n\
         const p = once(ee, 'done');\n\
         if (typeof p?.then !== 'function') throw new Error('once promise');\n\
         if (listenerCount(ee, 'done') !== 1) throw new Error('listenerCount');\n\
         setMaxListeners(25, ee);\n\
         if (ee.getMaxListeners() !== 25) throw new Error('setMaxListeners');\n\
         ee.emit('done', 1);\n\
         'ok';",
    );
}

#[test]
fn test_stream_pipeline_and_finished() {
    let mut otter = full_engine_for_parity();
    assert_ok(
        &mut otter,
        "import { Readable, Writable, pipeline, finished } from 'node:stream';\n\
         let out = '';\n\
         const source = new Readable();\n\
         const sink = new Writable();\n\
         sink.write = (chunk) => { out += String(chunk); return true; };\n\
         sink.end = () => { sink.emit('finish'); };\n\
         let piped = false;\n\
         pipeline(source, sink, (err) => { if (err) throw err; piped = true; });\n\
         source.push('a');\n\
         source.push('b');\n\
         source.push(null);\n\
         if (out !== 'ab') throw new Error('pipeline output');\n\
         if (!piped) throw new Error('pipeline callback');\n\
         let done = false;\n\
         finished(sink, (err) => { if (err) throw err; done = true; });\n\
         sink.emit('finish');\n\
         if (!done) throw new Error('finished callback');\n\
         'ok';",
    );
}

#[test]
fn test_buffer_extended_api_surface() {
    let mut otter = full_engine_for_parity();
    assert_ok(
        &mut otter,
        "import { Buffer } from 'node:buffer';\n\
         if (typeof Buffer !== 'function') throw new Error('bufferType:' + String(typeof Buffer));\n\
         const raw = Buffer.allocUnsafe(4);\n\
         if (!raw) throw new Error('allocUnsafe:null');\n\
         const a = raw.fill(0x61);\n\
         if (!a) throw new Error('fill:null');\n\
         const s = a.toString('utf8');\n\
         if (s !== 'aaaa') throw new Error('allocUnsafe/fill:' + String(s) + ':len=' + a.length);\n\
         if (!Buffer.isEncoding('hex')) throw new Error('isEncoding');\n\
         if (Buffer.compare(Buffer.from('a'), Buffer.from('b')) >= 0) throw new Error('compare');\n\
         if (typeof Buffer.poolSize !== 'number') throw new Error('poolSize');\n\
         'ok';",
    );
}

#[test]
fn test_buffer_search_apis() {
    let mut otter = full_engine_for_parity();
    assert_ok(
        &mut otter,
        "import { Buffer } from 'node:buffer';\n\
         if (typeof Buffer !== 'function') throw new Error('bufferType:' + String(typeof Buffer));\n\
         const b = Buffer.from('hello');\n\
         if (!b) throw new Error('from:null');\n\
         const inc = b.includes('ell');\n\
         const i = b.indexOf('l');\n\
         const li = b.lastIndexOf('l');\n\
         if (!inc) throw new Error('includes:' + String(inc) + ':' + String(i) + ':' + String(li));\n\
         if (i !== 2) throw new Error('indexOf:' + String(i));\n\
         if (li !== 3) throw new Error('lastIndexOf:' + String(li));\n\
         'ok';",
    );
}

#[test]
fn test_path_extended_api_surface() {
    let mut otter = full_engine_for_parity();
    assert_ok(
        &mut otter,
        "import path from 'node:path';\n\
         if (path.toNamespacedPath('/tmp/x') !== '/tmp/x') throw new Error('toNamespacedPath');\n\
         if (path.posix.sep !== '/') throw new Error('posix.sep');\n\
         if (path.win32.delimiter !== ';') throw new Error('win32.delimiter');\n\
         const parsed = path.parse('/tmp/a.txt');\n\
         if (parsed.base !== 'a.txt') throw new Error('parse');\n\
         const formatted = path.format({ dir: '/tmp', name: 'a', ext: '.txt' });\n\
         if (!formatted.endsWith('a.txt')) throw new Error('format');\n\
         if (typeof path.relative('/tmp', '/tmp/a') !== 'string') throw new Error('relative');\n\
         'ok';",
    );
}
