'use strict';

// The program's console is Node's console.
//
// What it prints goes through `process.stdout` and `process.stderr`, which is
// what lets a program put its own stream in their place and read back what it
// printed. It is built the first time it is looked at, so a program that never
// prints pays nothing for the module behind it — and until then the intrinsic
// console the realm starts with is what answers.
{
  const install = (value) => {
    Object.defineProperty(globalThis, 'console', {
      value,
      writable: true,
      enumerable: false,
      configurable: true,
    });
    return value;
  };
  Object.defineProperty(globalThis, 'console', {
    get() {
      const value = process.getBuiltinModule('console');
      // Node's own bootstrap does this: the console the module builds is not
      // bound to any streams until the process it belongs to is named.
      process.getBuiltinModule('internal/console/constructor')
        .initializeGlobalConsole(value);
      return install(value);
    },
    set(value) {
      install(value);
    },
    enumerable: false,
    configurable: true,
  });
}
