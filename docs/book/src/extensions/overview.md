# Extensions Overview

Otter's extension model is layered:

1. hosted modules inside the workspace;
2. native bindings compiled with the engine;
3. future out-of-tree plugin packages;
4. possible future ABI/FFI boundary for dynamically loaded plugins.

All layers must preserve the same runtime rules:

- permissions are deny-by-default;
- no raw GC handle crosses isolate or worker boundaries;
- persistent JS-visible state uses `Root`;
- weak handles upgrade only through a matching context;
- external memory is accounted;
- async work hops back to the isolate before touching VM state.

The future plugin system is tracked in the new-engine task files until
the API is stable enough to document here fully.
