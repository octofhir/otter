'use strict';
// Console methods the standard defines on top of the engine's write path.
//
// The engine owns the seven that actually write — log, info, debug, warn,
// error, trace, assert — because they route through the embedder's sink. The
// rest are bookkeeping and formatting expressed in terms of those, so they live
// here rather than in the intrinsic.
(function installConsoleExtras(globalThis) {
  const console = globalThis.console;
  if (!console || typeof console.log !== 'function') return;

  const write = console.log.bind(console);
  const writeError = (console.error || console.log).bind(console);

  const counts = new Map();
  const timers = new Map();
  let groupDepth = 0;

  // Every line of grouped output carries the group's indentation.
  function emit(sink, args) {
    if (groupDepth === 0) {
      sink(...args);
      return;
    }
    const indent = '  '.repeat(groupDepth);
    const [first, ...rest] = args;
    if (typeof first === 'string') {
      sink(indent + first, ...rest);
    } else {
      sink(indent, ...args);
    }
  }

  const base = { log: console.log, info: console.info, debug: console.debug,
                 warn: console.warn, error: console.error, trace: console.trace };
  for (const name of Object.keys(base)) {
    const original = base[name];
    if (typeof original !== 'function') continue;
    console[name] = function (...args) {
      emit((...out) => original.apply(console, out), args);
    };
  }

  console.dir = function dir(item, options) {
    void options;
    emit(write, [item]);
  };

  console.dirxml = function dirxml(...args) {
    emit(write, args);
  };

  console.group = function group(...args) {
    if (args.length > 0) emit(write, args);
    groupDepth += 1;
  };
  console.groupCollapsed = console.group;
  console.groupEnd = function groupEnd() {
    if (groupDepth > 0) groupDepth -= 1;
  };

  console.count = function count(label = 'default') {
    const key = String(label);
    const next = (counts.get(key) ?? 0) + 1;
    counts.set(key, next);
    emit(write, [`${key}: ${next}`]);
  };
  console.countReset = function countReset(label = 'default') {
    counts.delete(String(label));
  };

  console.time = function time(label = 'default') {
    const key = String(label);
    if (timers.has(key)) {
      writeError(`Warning: Label '${key}' already exists for console.time()`);
      return;
    }
    timers.set(key, Date.now());
  };
  function elapsed(key) {
    const started = timers.get(key);
    if (started === undefined) {
      writeError(`Warning: No such label '${key}' for console.timeEnd()`);
      return null;
    }
    return Date.now() - started;
  }
  console.timeLog = function timeLog(label = 'default', ...rest) {
    const key = String(label);
    const duration = elapsed(key);
    if (duration === null) return;
    emit(write, [`${key}: ${duration}ms`, ...rest]);
  };
  console.timeEnd = function timeEnd(label = 'default') {
    const key = String(label);
    const duration = elapsed(key);
    if (duration === null) return;
    timers.delete(key);
    emit(write, [`${key}: ${duration}ms`]);
  };

  // A table of rows keyed by index (or by object key), with one column per
  // property seen across the rows. Non-tabular input falls back to logging.
  console.table = function table(data, columns) {
    if (data === null || typeof data !== 'object') {
      emit(write, [data]);
      return;
    }
    const isArray = Array.isArray(data);
    const keys = isArray ? data.map((_, index) => String(index)) : Object.keys(data);
    const rows = keys.map((key) => (isArray ? data[Number(key)] : data[key]));

    const valueColumns = [];
    let hasValues = false;
    for (const row of rows) {
      if (row !== null && typeof row === 'object') {
        for (const column of Object.keys(row)) {
          if (!valueColumns.includes(column)) valueColumns.push(column);
        }
      } else {
        hasValues = true;
      }
    }
    const shown = Array.isArray(columns)
      ? columns.map(String)
      : valueColumns;
    const header = [isArray ? '(index)' : '(index)', ...shown];
    if (hasValues) header.push('Values');

    const cell = (value) => {
      if (value === undefined) return '';
      if (typeof value === 'string') return `'${value}'`;
      return String(value);
    };
    const body = keys.map((key, index) => {
      const row = rows[index];
      const line = [key];
      for (const column of shown) {
        line.push(row !== null && typeof row === 'object' ? cell(row[column]) : '');
      }
      if (hasValues) line.push(row !== null && typeof row === 'object' ? '' : cell(row));
      return line;
    });

    const widths = header.map((title, column) =>
      Math.max(title.length, ...body.map((line) => line[column].length), 0));
    const rule = (left, mid, right) =>
      left + widths.map((width) => '─'.repeat(width + 2)).join(mid) + right;
    const format = (line) =>
      '│' + line.map((value, column) => ` ${value.padEnd(widths[column])} `).join('│') + '│';

    emit(write, [rule('┌', '┬', '┐')]);
    emit(write, [format(header)]);
    emit(write, [rule('├', '┼', '┤')]);
    for (const line of body) emit(write, [format(line)]);
    emit(write, [rule('└', '┴', '┘')]);
  };

  console.clear = function clear() {};
  console.profile = function profile() {};
  console.profileEnd = function profileEnd() {};
  console.timeStamp = function timeStamp() {};

  // `new Console({ stdout, stderr })` writes to the given streams instead of
  // the engine's sink; every derived method above is shared by construction.
  class Console {
    constructor(options, maybeStderr) {
      const stdout = options && typeof options === 'object' && !options.write
        ? options.stdout
        : options;
      const stderr = options && typeof options === 'object' && !options.write
        ? (options.stderr ?? stdout)
        : (maybeStderr ?? stdout);
      if (!stdout || typeof stdout.write !== 'function') {
        const err = new TypeError(
          'The "options.stdout" property must be of type object. Received ' +
            (stdout === null ? 'null' : typeof stdout));
        err.code = 'ERR_INVALID_ARG_TYPE';
        throw err;
      }

      const render = (args) => args
        .map((value) => (typeof value === 'string' ? value : String(value)))
        .join(' ') + '\n';
      const toOut = (...args) => stdout.write(render(args));
      const toErr = (...args) => (stderr ?? stdout).write(render(args));

      this.log = toOut;
      this.info = toOut;
      this.debug = toOut;
      this.dir = (item) => stdout.write(render([item]));
      this.warn = toErr;
      this.error = toErr;
      this.trace = toErr;
      this.assert = (condition, ...args) => {
        if (!condition) toErr('Assertion failed:', ...args);
      };
      for (const name of ['group', 'groupCollapsed', 'groupEnd', 'table', 'time',
                          'timeEnd', 'timeLog', 'count', 'countReset', 'clear',
                          'dirxml', 'profile', 'profileEnd', 'timeStamp']) {
        this[name] = console[name];
      }
    }
  }

  // `globalThis.console instanceof Console` holds in Node, and the global is
  // not a real instance here, so the check is answered by the class.
  Object.defineProperty(Console, Symbol.hasInstance, {
    value: (instance) => instance === console || Object.getPrototypeOf(instance) === Console.prototype,
    configurable: true,
  });

  console.Console = Console;
  globalThis.Console = Console;
})(globalThis);
