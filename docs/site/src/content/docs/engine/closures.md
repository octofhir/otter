---
title: Closure capture storage
description: Shared bindings, inline capture arrays and native call windows.
---

## Heap ownership

A closure is one old-space GC allocation. Its fixed body carries the function
identity, lexical receiver state, eval environment and function-object metadata.
A trailing array carries four-byte compressed references to captured binding
cells. There is no separately allocated capture-array object.

Binding cells hold the authoritative JavaScript values. Sibling closures and
mapped arguments share cells; a loop binding renewal replaces the relevant
reference. Flattening the capture array does not copy the binding values or
change their sharing semantics.

## Construction and collection

The allocator borrows a mutable slice of capture references. VM construction
uses an inline buffer for up to 16 captures; larger environments spill into a
host allocation without changing the heap representation.

Before allocation, fixed closure fields and the input capture buffer are roots.
The pending body has no trailing storage, so its pending tracer walks only fixed
fields. After allocation, the initializer copies the current capture references
into the tail before the allocator records outgoing edges. The normal tracer
visits the actual tail slots, allowing collection to rewrite moving children.

Both allocation entry points use this same initialization and old-space policy.
Native and VM call windows borrow the capture array while rooting its exact
closure owner. The array remains at a stable address during execution. Snapshot
restore relocates the owning closure and recomputes the call header's derived
capture address before execution resumes.

## Validation

Closure tests force collection while the payload is pending and move a captured
child through two minor collections. Runtime snapshot coverage drops the donor,
collects the restored heap, and calls sibling closures through tier-up while
checking that updates remain shared. The differential capture corpus additionally
covers loop renewal, mapped arguments, direct eval and temporal dead zones.
