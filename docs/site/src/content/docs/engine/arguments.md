---
title: "Arguments objects and activation reads"
---

Otter retains each function's actual argument list independently of its formal
parameters. Extra arguments remain available; missing actuals are never invented.
The interpreter stores this list in the frame's cold state. Generated callers
publish a traced argument window immediately after the callee's register window.

## Local reads

After AST lowering, the compiler follows copies of the implicit `arguments`
identity through registers and locals. If every use is a copy, a `.length` read,
or an indexed read, it replaces object construction and those consumers with
`LoadArgumentsLength` and `LoadArgumentsElement`.

This proof currently admits zero-formal functions without rest parameters,
direct eval, suspension or exception-region control flow. Escaping identities
and receivers that merge with ordinary values retain canonical construction.
Mapped parameter functions and captured aliases also retain the ordinary object.
Admission is independent of function names. The bytecode verifier checks the
required function metadata before execution.

The interpreter and both JIT tiers read length and exact in-range Int32 indices
from the actual argument list. Machine IR contains an explicit
`ArgumentsReadProbe`, its committed cold call, Success/Throw/Fatal edges and the
result join before register allocation. Artifact code maps identify the probe
as `machineArgumentsReadProbe` at the source byte PC.

## Observable materialization

Other keys and out-of-range indices materialize the canonical arguments object
before ordinary property access. This preserves key coercion, inherited getters,
exceptions and symbol properties. A getter may expose or mutate its receiver;
every subsequent arguments operation in the activation uses that same object.
The operation completes once and is never replayed after a cold call.

The native activation owns a nullable compressed object root at byte offset 52
of its 72-byte `NativeFrame`. Every generated entry initializes it before
publication. The collector rewrites it in place. Native entry, interpreter
continuations and exact deoptimization preserve the current identity. The
runtime roots property keys before materialization can allocate. Generated
probes never retain a raw argument-window address across reentry or a backedge.

Arguments objects receive the original realm-owned `%Array.prototype.values%`
intrinsic as their iterator method. Changing `Array.prototype.values` does not
change arguments creation. The retained callable is traced and included in the
snapshot root walk.

The existing apply-only proof uses `CallForwardArguments` and the same actual
window. Once an object has materialized, forwarding reads its live properties
through the canonical array-like argument collection operation.
