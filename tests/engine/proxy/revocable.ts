/* otter-test:
name = "proxy: Proxy.revocable produces a revoke pair"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}

// §28.2.2 — `Proxy.revocable(target, handler)` returns
// `{ proxy, revoke }`.
const r = Proxy.revocable({ a: 1 }, { get: (t, k) => t[k] });
if (typeof r.revoke !== "function") fail();
if (r.proxy.a !== 1) fail();

r.revoke();

// After revoke, every trap raises a TypeError.
let threw = false;
try { r.proxy.a; } catch (e) { threw = true; }
if (!threw) fail();

// Re-revoking is idempotent (no error).
r.revoke();
