'use strict';

// `Buffer` is a global in every Node program. The module that defines it is
// loaded the first time a program names it, so nothing loads it at startup;
// loading it replaces this accessor with the class itself.
{
  const install = () => process.getBuiltinModule('buffer').Buffer;
  Object.defineProperty(globalThis, 'Buffer', {
    get: install,
    set(value) {
      install();
      Object.defineProperty(globalThis, 'Buffer', {
        value,
        writable: true,
        enumerable: false,
        configurable: true,
      });
    },
    enumerable: false,
    configurable: true,
  });
}
