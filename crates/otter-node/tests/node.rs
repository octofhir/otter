use otter_node::{NodeApiBuilderExt, hosted_modules};
use otter_runtime::{
    CapabilitySet, Permission, ResourceAccount, ResourceClass, ResourceLimits, Runtime,
};

#[test]
fn hosted_node_module_specs_are_static_and_ordered() {
    let specs = hosted_modules();
    // `fs` leads the list; the exact length grows as modules are ported.
    assert_eq!(specs[0].specifier(), "node:fs");
    assert_eq!(specs[1].specifier(), "fs");

    // Every `node:`-prefixed builtin has a matching bare specifier, except
    // the ones only the scheme reaches — a bare row for those would shadow a
    // `node_modules` package of the same name.
    let names: Vec<&str> = specs.iter().map(|m| m.specifier()).collect();
    for name in &names {
        if let Some(bare) = name.strip_prefix("node:") {
            if otter_runtime::SCHEME_ONLY_BUILTINS.contains(&bare) {
                assert!(
                    !names.contains(&bare),
                    "{name} must not be reachable without the scheme"
                );
                continue;
            }
            assert!(names.contains(&bare), "missing bare specifier for {name}");
        }
    }
    // Core modules ported so far are registered.
    for expected in ["os", "node:os", "node:test", "assert", "path"] {
        assert!(
            names.contains(&expected),
            "missing hosted module {expected}"
        );
    }
}

#[test]
fn node_fs_requires_read_permission() {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("main.mjs");
    let data = dir.path().join("data.txt");
    std::fs::write(&data, "secret").unwrap();
    std::fs::write(
        &main,
        r#"
            import { readFileSync } from "node:fs";
            readFileSync("data.txt", "utf8");
        "#,
    )
    .unwrap();

    let mut runtime = Runtime::builder().with_node_apis().build().unwrap();
    let err = runtime.run_module(&main).unwrap_err();
    assert!(err.to_string().contains("permission denied"));
}

#[test]
fn node_fs_read_write_round_trips_with_permissions() {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("main.mjs");
    let data = dir.path().join("data.txt");
    std::fs::write(
        &main,
        format!(
            r#"
            import {{ existsSync, readFileSync, writeFileSync }} from "node:fs";
            writeFileSync({path:?}, "hello", "utf8");
            if (!existsSync({path:?})) {{
                throw new Error("exists failed");
            }}
            if (readFileSync({path:?}, "utf8") !== "hello") {{
                throw new Error("read failed");
            }}
        "#,
            path = data.to_string_lossy()
        ),
    )
    .unwrap();
    let caps = CapabilitySet {
        read: Permission::allow([data.clone()]),
        write: Permission::allow([data]),
        ..CapabilitySet::sandbox()
    };

    let mut runtime = Runtime::builder()
        .capabilities(caps)
        .with_node_apis()
        .build()
        .unwrap();
    runtime.run_module(&main).unwrap();
}

/// The filesystem members added for Node's own tests: a private temporary
/// directory, both link kinds, ownership-free timestamp changes, and the
/// descriptor operations that used to be silent no-ops.
#[test]
fn node_fs_links_timestamps_and_descriptor_operations() {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("main.mjs");
    std::fs::write(
        &main,
        format!(
            r#"
            import {{
                closeSync, existsSync, fsyncSync, ftruncateSync, linkSync, mkdtempSync,
                openSync, readFileSync, readlinkSync, statSync, symlinkSync, utimesSync,
                writeFileSync,
            }} from "node:fs";
            import {{ join }} from "node:path";

            const root = {root:?};
            const scratch = mkdtempSync(join(root, "otter-"));
            if (!existsSync(scratch)) throw new Error("mkdtemp did not create a directory");
            if (!scratch.startsWith(join(root, "otter-"))) {{
                throw new Error("mkdtemp ignored its prefix: " + scratch);
            }}
            if (mkdtempSync(join(root, "otter-")) === scratch) {{
                throw new Error("mkdtemp handed out the same name twice");
            }}

            const file = join(scratch, "a.txt");
            writeFileSync(file, "hello");
            const hard = join(scratch, "hard.txt");
            linkSync(file, hard);
            if (readFileSync(hard, "utf8") !== "hello") throw new Error("hard link lost content");

            const soft = join(scratch, "soft.txt");
            symlinkSync(file, soft);
            if (readlinkSync(soft) !== file) throw new Error("symlink target: " + readlinkSync(soft));

            utimesSync(file, 1600000000, 1600000001);
            const stamped = statSync(file);
            if (Math.round(stamped.mtimeMs / 1000) !== 1600000001) {{
                throw new Error("mtime was " + stamped.mtimeMs);
            }}

            const fd = openSync(file, "r+");
            ftruncateSync(fd, 1);
            fsyncSync(fd);
            closeSync(fd);
            if (readFileSync(file, "utf8") !== "h") {{
                throw new Error("ftruncate left " + readFileSync(file, "utf8"));
            }}
        "#,
            root = dir.path().to_string_lossy()
        ),
    )
    .unwrap();

    let mut runtime = Runtime::builder()
        .capabilities(CapabilitySet::allow_all())
        .with_node_apis()
        .build()
        .unwrap();
    runtime.run_module(&main).unwrap();
}

/// `fsPromises.open` hands out a handle whose reads, writes, metadata, and
/// web stream all address the same descriptor, and whose methods refuse to run
/// once it is closed.
#[test]
fn node_fs_promises_file_handle_round_trips() {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("main.mjs");
    let data = dir.path().join("data.txt");
    std::fs::write(&data, "hello world").unwrap();
    std::fs::write(
        &main,
        format!(
            r#"
            import {{ open }} from "node:fs/promises";

            const handle = await open({path:?}, "r");
            if (typeof handle.fd !== "number") throw new Error("fd was " + typeof handle.fd);

            const {{ bytesRead, buffer }} = await handle.read(Buffer.alloc(5), 0, 5, 0);
            if (bytesRead !== 5) throw new Error("bytesRead was " + bytesRead);
            if (buffer.toString("utf8", 0, bytesRead) !== "hello") {{
                throw new Error("read " + buffer.toString("utf8", 0, bytesRead));
            }}

            const stat = await handle.stat();
            if (stat.size !== 11) throw new Error("size was " + stat.size);

            await handle.close();
            let afterClose;
            try {{
                await handle.read(Buffer.alloc(1), 0, 1, 0);
            }} catch (error) {{
                afterClose = error;
            }}
            if (afterClose?.code !== "EBADF") {{
                throw new Error("a closed handle answered " + afterClose?.code);
            }}
            await handle.close();
        "#,
            path = data.to_string_lossy()
        ),
    )
    .unwrap();

    // `fs.promises` completes its work on the event loop, so this one runs
    // on the loop-driving runtime rather than the bare synchronous one.
    let otter = otter_runtime::Otter::builder()
        .capabilities(CapabilitySet::allow_all())
        .with_node_apis()
        .build()
        .unwrap();
    otter.blocking_run_module(&main).unwrap();
}

/// Key derivation answers the RFC vectors byte for byte, and its argument
/// checks carry the codes Node's own tests assert.
#[test]
fn node_crypto_key_derivation_matches_the_published_vectors() {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("main.mjs");
    std::fs::write(
        &main,
        r#"
        import { pbkdf2Sync, hkdfSync } from "node:crypto";

        // RFC 6070 PBKDF2-HMAC-SHA1.
        const first = pbkdf2Sync("password", "salt", 1, 20, "sha1").toString("hex");
        if (first !== "0c60c80f961f0e71f3a9b524af6012062fe037a6") {
            throw new Error("pbkdf2 sha1: " + first);
        }
        const long = pbkdf2Sync(
            "passwordPASSWORDpassword",
            "saltSALTsaltSALTsaltSALTsaltSALTsalt", 4096, 25, "sha1").toString("hex");
        if (long !== "3d2eec4fe41c849b80c8d83662c0e44a8b291a964cf2f07038") {
            throw new Error("pbkdf2 long: " + long);
        }
        const sha256 = pbkdf2Sync("password", "salt", 4096, 32, "sha256").toString("hex");
        if (sha256 !== "c5e478d59288c841aa530db6845c4c8d962893a001ce4e11a4963873aa98134a") {
            throw new Error("pbkdf2 sha256: " + sha256);
        }

        const derived = Buffer.from(hkdfSync("sha256", "secret", "salt", "info", 42))
            .toString("hex");
        if (derived.length !== 84) throw new Error("hkdf length: " + derived.length);

        const checks = [
            [() => pbkdf2Sync("p", "s", 0, 20, "sha1"), "ERR_OUT_OF_RANGE"],
            [() => pbkdf2Sync("p", "s", 1, "20", "sha1"), "ERR_INVALID_ARG_TYPE"],
            [() => pbkdf2Sync("p", "s", 1, 20), "ERR_INVALID_ARG_TYPE"],
            [() => pbkdf2Sync("p", "s", 1, 20, "md55"), "ERR_CRYPTO_INVALID_DIGEST"],
            [() => pbkdf2Sync(1, "s", 1, 20, "sha1"), "ERR_INVALID_ARG_TYPE"],
        ];
        for (const [call, code] of checks) {
            let thrown;
            try { call(); } catch (error) { thrown = error; }
            if (thrown?.code !== code) throw new Error(code + " got " + thrown?.code);
        }
        "#,
    )
    .unwrap();

    let mut runtime = Runtime::builder()
        .capabilities(CapabilitySet::allow_all())
        .with_node_apis()
        .build()
        .unwrap();
    runtime.run_module(&main).unwrap();
}

/// A store entered with `AsyncLocalStorage.run` is still current inside the
/// continuations scheduled from that scope, and absent outside it. The timer
/// leg needs a host scheduler, so it is covered by the Node corpus rather than
/// here.
#[test]
fn node_async_local_storage_survives_every_continuation() {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("main.mjs");
    std::fs::write(
        &main,
        r#"
        import { AsyncLocalStorage } from "node:async_hooks";

        const storage = new AsyncLocalStorage();
        const seen = [];

        await storage.run({ id: 7 }, async () => {
            seen.push(["sync", storage.getStore()?.id]);
            const microtask = Promise.resolve().then(() => {
                seen.push(["microtask", storage.getStore()?.id]);
            });
            const resumed = (async () => {
                await null;
                seen.push(["await", storage.getStore()?.id]);
            })();
            await Promise.all([microtask, resumed]);
        });

        if (storage.getStore() !== undefined) throw new Error("the store escaped its scope");
        for (const [where, id] of seen) {
            if (id !== 7) throw new Error(where + " saw " + id);
        }
        if (seen.length !== 3) throw new Error("recorded " + JSON.stringify(seen));

        // A second store must not see the first one's binding.
        const other = new AsyncLocalStorage();
        storage.run({ id: 1 }, () => {
            if (other.getStore() !== undefined) throw new Error("stores are not independent");
        });
        "#,
    )
    .unwrap();

    let mut runtime = Runtime::builder()
        .capabilities(CapabilitySet::allow_all())
        .with_node_apis()
        .build()
        .unwrap();
    runtime.run_module(&main).unwrap();
}

/// A domain owns the errors of the work started inside it: a synchronous
/// throw, and one raised from a continuation the domain never sees directly.
#[test]
fn node_domain_claims_errors_from_its_own_work() {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("main.mjs");
    std::fs::write(
        &main,
        r#"
        import domain from "node:domain";

        const caught = [];
        const sync = domain.create();
        sync.on("error", (error) => caught.push(["sync", error.message]));
        sync.run(() => { throw new Error("boom"); });

        if (caught.length !== 1) throw new Error("sync throw was not claimed");
        if (caught[0][1] !== "boom") throw new Error("claimed " + caught[0][1]);

        const nested = domain.create();
        nested.run(() => {
            if (process.domain !== nested) throw new Error("process.domain is not the active one");
        });
        if (process.domain === nested) throw new Error("the domain outlived its run");

        const bound = domain.create();
        let boundError;
        bound.on("error", (error) => { boundError = error; });
        bound.bind(() => { throw new Error("bound"); })();
        if (boundError?.message !== "bound") throw new Error("bind did not route");

        const intercepted = domain.create();
        let interceptedError;
        intercepted.on("error", (error) => { interceptedError = error; });
        intercepted.intercept(() => { throw new Error("never runs"); })(new Error("first-arg"));
        if (interceptedError?.message !== "first-arg") throw new Error("intercept did not route");
        "#,
    )
    .unwrap();

    let mut runtime = Runtime::builder()
        .capabilities(CapabilitySet::allow_all())
        .with_node_apis()
        .build()
        .unwrap();
    runtime.run_module(&main).unwrap();
}

/// `dns` rejects the arguments Node rejects and exposes the platform's own
/// hint flags. Resolution itself needs a host timer to deliver its callback,
/// so it is covered by the Node corpus rather than here.
#[test]
fn node_dns_lookup_resolves_and_validates() {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("main.mjs");
    std::fs::write(
        &main,
        r#"
        import dns from "node:dns";

        let wrongType;
        try { dns.lookup(1, {}); } catch (error) { wrongType = error; }
        if (wrongType?.code !== "ERR_INVALID_ARG_TYPE") throw new Error("code " + wrongType?.code);
        if (wrongType.name !== "TypeError") throw new Error("name " + wrongType.name);

        let badOrder;
        try { dns.setDefaultResultOrder("sideways"); } catch (error) { badOrder = error; }
        if (badOrder?.code !== "ERR_INVALID_ARG_VALUE") throw new Error("code " + badOrder?.code);

        dns.setServers(["1.1.1.1"]);
        if (dns.getServers().join() !== "1.1.1.1") throw new Error("servers did not round-trip");

        // The hint flags are the platform's own, not invented ones.
        for (const flag of ["ADDRCONFIG", "V4MAPPED", "ALL"]) {
            if (typeof dns[flag] !== "number" || dns[flag] <= 0) {
                throw new Error(flag + " is " + dns[flag]);
            }
        }
        "#,
    )
    .unwrap();

    let mut runtime = Runtime::builder()
        .capabilities(CapabilitySet::allow_all())
        .with_node_apis()
        .build()
        .unwrap();
    runtime.run_module(&main).unwrap();
}

/// `dgram`'s synchronous surface: the socket type it accepts, and the errors it
/// raises before it touches a socket. Binding, sending, and receiving all park
/// their work on a timer, which a bare runtime has no scheduler for, so those
/// legs are covered by the Node corpus rather than here.
#[test]
fn node_dgram_validates_before_it_opens_a_socket() {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("main.mjs");
    std::fs::write(
        &main,
        r#"
        import dgram from "node:dgram";

        function codeOf(run) {
            try { run(); } catch (error) { return error.code; }
            return undefined;
        }

        if (codeOf(() => dgram.createSocket("udp5")) !== "ERR_SOCKET_BAD_TYPE") {
            throw new Error("bad type is not rejected");
        }
        if (codeOf(() => dgram.createSocket({})) !== "ERR_SOCKET_BAD_TYPE") {
            throw new Error("missing type is not rejected");
        }

        // A string and an options object name the same socket.
        for (const options of ["udp4", { type: "udp4" }]) {
            const socket = dgram.createSocket(options);
            if (socket.type !== "udp4") throw new Error("type " + socket.type);
            if (!(socket instanceof dgram.Socket)) throw new Error("not a Socket");
        }

        const socket = dgram.createSocket("udp6");
        if (socket.type !== "udp6") throw new Error("type " + socket.type);

        // Nothing is open yet. A never-bound socket has no descriptor, so
        // address() fails the way the getsockname syscall would; the
        // not-running state error is reserved for a socket closed later.
        if (codeOf(() => socket.address()) !== "EBADF") {
            throw new Error("address() on an unbound socket");
        }
        if (codeOf(() => socket.remoteAddress()) !== "ERR_SOCKET_DGRAM_NOT_CONNECTED") {
            throw new Error("remoteAddress() on an unconnected socket");
        }
        if (codeOf(() => socket.disconnect()) !== "ERR_SOCKET_DGRAM_NOT_CONNECTED") {
            throw new Error("disconnect() on an unconnected socket");
        }
        // The port is validated before the connection state, so a send
        // with neither reports the port.
        if (codeOf(() => socket.send("payload")) !== "ERR_SOCKET_BAD_PORT") {
            throw new Error("send() without a port on an unconnected socket");
        }

        // The port is checked before the connection state is.
        if (codeOf(() => socket.connect(0)) !== "ERR_SOCKET_BAD_PORT") {
            throw new Error("connect(0)");
        }
        if (codeOf(() => socket.connect(65536)) !== "ERR_SOCKET_BAD_PORT") {
            throw new Error("connect(65536)");
        }
        if (codeOf(() => socket.connectSync(1, "example.com")) !== "ERR_INVALID_ARG_VALUE") {
            throw new Error("connectSync with a name instead of an address");
        }
        if (codeOf(() => socket.bindSync({ address: "example.com" })) !== "ERR_INVALID_ARG_VALUE") {
            throw new Error("bindSync with a name instead of an address");
        }
        if (codeOf(() => socket.bindSync({ port: 65536 })) !== "ERR_SOCKET_BAD_PORT") {
            throw new Error("bindSync with an out-of-range port");
        }
        if (codeOf(() => socket.bindSync(null)) !== "ERR_INVALID_ARG_TYPE") {
            throw new Error("bindSync(null)");
        }

        // The numeric and string option setters type-check their argument
        // before reaching the socket.
        const wrongTypes = [
            () => socket.setTTL("2"),
            () => socket.setMulticastTTL("2"),
            () => socket.setMulticastInterface(1),
        ];
        for (const run of wrongTypes) {
            if (codeOf(run) !== "ERR_INVALID_ARG_TYPE") {
                throw new Error("option setter accepted a wrong type: " + run);
            }
        }
        // Membership takes its address to the socket, which has none yet.
        for (const run of [() => socket.addMembership(1), () => socket.dropMembership(1)]) {
            if (codeOf(run) !== "EINVAL") {
                throw new Error("membership on an unbound socket: " + run);
            }
        }
        // A broadcast flag is not type-checked; the unbound socket is what
        // refuses it.
        if (codeOf(() => socket.setBroadcast(1)) !== "EBADF") {
            throw new Error("setBroadcast on an unbound socket");
        }
        "#,
    )
    .unwrap();

    let mut runtime = Runtime::builder()
        .capabilities(CapabilitySet::allow_all())
        .with_node_apis()
        .build()
        .unwrap();
    runtime.run_module(&main).unwrap();
}

/// `cluster` reports which role the process is in, and the primary's surface
/// is the one a program gets when nothing launched it as a worker. Forking a
/// worker starts a process and waits on a channel, neither of which a bare
/// runtime can do, so that half is covered by running the CLI.
#[test]
fn node_cluster_reports_the_primary_role() {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("main.mjs");
    std::fs::write(
        &main,
        r#"
        import cluster from "node:cluster";

        if (cluster.isWorker) throw new Error("a process nobody forked is not a worker");
        if (!cluster.isPrimary) throw new Error("isPrimary " + cluster.isPrimary);
        if (cluster.isMaster !== cluster.isPrimary) throw new Error("isMaster disagrees");
        if (cluster.worker !== undefined) throw new Error("a primary is not a worker");

        // The primary keeps a registry, and a worker has none to keep.
        if (typeof cluster.workers !== "object" || cluster.workers === null) {
            throw new Error("workers registry is " + typeof cluster.workers);
        }
        if (Object.keys(cluster.workers).length !== 0) throw new Error("registry is not empty");

        for (const name of ["fork", "disconnect", "setupPrimary", "setupMaster"]) {
            if (typeof cluster[name] !== "function") throw new Error(name + " is missing");
        }
        if (typeof cluster.Worker !== "function") throw new Error("Worker class is missing");

        // Settings name the program a worker would run, which is this one.
        // `setupPrimary` records them on the module rather than answering
        // with them.
        cluster.setupPrimary();
        const settings = cluster.settings;
        if (settings.exec !== process.argv[1]) throw new Error("exec is " + settings.exec);

        // A primary schedules round-robin unless told otherwise, and the
        // policy names are the ones a program compares against.
        if (cluster.SCHED_RR === cluster.SCHED_NONE) throw new Error("policies collide");
        if (cluster.schedulingPolicy !== cluster.SCHED_RR) {
            throw new Error("policy " + cluster.schedulingPolicy);
        }
        "#,
    )
    .unwrap();

    let mut runtime = Runtime::builder()
        .capabilities(CapabilitySet::allow_all())
        .with_node_apis()
        .build()
        .unwrap();
    runtime.run_module(&main).unwrap();
}

#[cfg(unix)]
#[test]
fn child_process_ipc_pressure_reports_enobufs() {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("main.mjs");
    std::fs::write(
        &main,
        r#"
        import { spawn } from "node:child_process";

        const child = spawn("/bin/sh", ["-c", "sleep 0.1"], {
          stdio: ["ignore", "ignore", "ignore", "ipc"],
        });
        const code = await new Promise((resolve) => {
          const accepted = child.send({ payload: "x" }, (error) => resolve(error?.code));
          if (accepted) throw new Error("over-budget IPC message was accepted");
        });
        if (code !== "ENOBUFS") throw new Error("IPC pressure was " + code);
        "#,
    )
    .unwrap();
    let account = ResourceAccount::new(
        ResourceLimits::builder()
            .limit(ResourceClass::QueuedMessageBytes, 2)
            .build(),
    );
    let otter = otter_runtime::Otter::builder()
        .resource_account(account.clone())
        .capabilities(CapabilitySet::allow_all())
        .with_node_apis()
        .build()
        .unwrap();

    otter.blocking_run_module(&main).unwrap();
    let snapshot = account.snapshot();
    let bytes = snapshot.get(ResourceClass::QueuedMessageBytes);
    assert_eq!(bytes.current(), 0);
    assert!(bytes.rejections() >= 1);
}

/// `process` is an EventEmitter in its own right: the runtime installs one and
/// nothing replaces it, so the listener methods, the meta-events, and the
/// `events` helpers all address the same object.
#[test]
fn node_process_is_the_only_event_emitter_it_has() {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("main.mjs");
    std::fs::write(
        &main,
        r#"
        import events from "node:events";

        // Requiring `events` must not hand `process` a second emitter.
        const before = process.on;
        await import("node:events");
        if (process.on !== before) throw new Error("process.on was replaced");

        const seen = [];
        const handler = () => {};
        process.on("newListener", (name, listener) => {
            if (name === "removeListener") return;
            // Node announces the addition before it takes effect.
            seen.push(`new:${name}:${process.listenerCount(name)}:${listener === handler}`);
        });
        process.on("removeListener", (name) => seen.push(`gone:${name}`));

        process.on("alpha", handler);
        if (process.listenerCount("alpha") !== 1) throw new Error("listener was not added");
        process.removeListener("alpha", handler);
        if (process.listenerCount("alpha") !== 0) throw new Error("listener was not removed");

        const expected = ["new:alpha:0:true", "gone:alpha"];
        if (seen.join("|") !== expected.join("|")) throw new Error("meta events: " + seen.join("|"));

        // Removing a listener that was never added announces nothing.
        seen.length = 0;
        process.removeListener("alpha", handler);
        if (seen.length !== 0) throw new Error("a removal that did nothing was announced");

        // The `events` helpers work against it, which is what having one
        // emitter buys.
        const settled = events.once(process, "beta");
        process.emit("beta", 1, 2);
        const args = await settled;
        if (args.join() !== "1,2") throw new Error("events.once got " + args.join());

        process.setMaxListeners(5);
        if (process.getMaxListeners() !== 5) throw new Error("max " + process.getMaxListeners());
        "#,
    )
    .unwrap();

    let mut runtime = Runtime::builder()
        .capabilities(CapabilitySet::allow_all())
        .with_node_apis()
        .build()
        .unwrap();
    runtime.run_module(&main).unwrap();
}

/// `net`'s address classification and the shapes it exposes without opening
/// anything. Listening, connecting, and carrying bytes all need the host's IO
/// runtime, which a bare runtime has none of, so those are CLI-verified.
#[test]
fn node_net_classifies_addresses_and_exposes_its_shapes() {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("main.mjs");
    std::fs::write(
        &main,
        r#"
        import net from "node:net";

        for (const name of ["createServer", "createConnection", "connect", "isIP"]) {
            if (typeof net[name] !== "function") throw new Error(name + " is missing");
        }
        if (typeof net.Server !== "function") throw new Error("Server is missing");
        if (typeof net.Socket !== "function") throw new Error("Socket is missing");

        // A version is reported as the number naming it, and anything else as 0.
        const addresses = [
            ["127.0.0.1", 4], ["0.0.0.0", 4], ["255.255.255.255", 4],
            ["::1", 6], ["::", 6], ["2001:db8::1", 6],
            ["256.0.0.1", 0], ["1.2.3", 0], ["", 0], ["nope", 0], ["1.2.3.4.5", 0],
        ];
        for (const [address, version] of addresses) {
            if (net.isIP(address) !== version) {
                throw new Error(`isIP(${address}) is ${net.isIP(address)}, not ${version}`);
            }
        }
        if (!net.isIPv4("1.2.3.4") || net.isIPv4("::1")) throw new Error("isIPv4 disagrees");
        if (!net.isIPv6("::1") || net.isIPv6("1.2.3.4")) throw new Error("isIPv6 disagrees");

        // A server that was never told to listen has no address, which is what
        // a program checks before using one. Closing it reports the refusal on
        // a later turn, so that leg belongs with the CLI checks.
        const server = net.createServer();
        if (server.listening) throw new Error("a fresh server is listening");
        if (server.address() !== null) throw new Error("a fresh server has an address");

        // A socket that was never connected is pending and carries no peer.
        const socket = new net.Socket();
        if (!socket.pending) throw new Error("a fresh socket is not pending");
        if (socket.connecting) throw new Error("a fresh socket is connecting");
        if (socket.remoteAddress !== undefined) throw new Error("a fresh socket has a peer");
        if (typeof socket.write !== "function" || typeof socket.pipe !== "function") {
            throw new Error("a socket is not a stream");
        }
        "#,
    )
    .unwrap();

    let mut runtime = Runtime::builder()
        .capabilities(CapabilitySet::allow_all())
        .with_node_apis()
        .build()
        .unwrap();
    runtime.run_module(&main).unwrap();
}

#[test]
fn node_tcp_and_udp_payloads_round_trip_and_release_queue_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("main.mjs");
    std::fs::write(
        &main,
        r#"
        import net from "node:net";
        import dgram from "node:dgram";

        const tcpServer = net.createServer((socket) => {
          socket.on('data', (chunk) => socket.end(Buffer.concat([Buffer.from('tcp:'), chunk])));
        });
        await new Promise((resolve, reject) => {
          tcpServer.once('error', reject);
          tcpServer.listen(0, '127.0.0.1', resolve);
        });
        const tcpAddress = tcpServer.address();
        const tcpReply = await new Promise((resolve, reject) => {
          const chunks = [];
          const socket = net.connect(tcpAddress.port, '127.0.0.1', () => socket.write('otter'));
          socket.on('data', (chunk) => chunks.push(chunk));
          socket.once('end', () => resolve(Buffer.concat(chunks).toString('utf8')));
          socket.once('error', reject);
        });
        if (tcpReply !== 'tcp:otter') throw new Error('TCP reply: ' + tcpReply);
        await new Promise((resolve, reject) => tcpServer.close((error) => error ? reject(error) : resolve()));

        const receiver = dgram.createSocket('udp4');
        await new Promise((resolve, reject) => {
          receiver.once('error', reject);
          receiver.bind(0, '127.0.0.1', resolve);
        });
        const udpAddress = receiver.address();
        const sender = dgram.createSocket('udp4');
        const udpReply = new Promise((resolve, reject) => {
          receiver.once('message', (payload) => resolve(payload.toString('utf8')));
          receiver.once('error', reject);
        });
        await new Promise((resolve, reject) => {
          sender.send(Buffer.from('datagram'), udpAddress.port, '127.0.0.1', (error) => {
            if (error) reject(error); else resolve();
          });
        });
        if (await udpReply !== 'datagram') throw new Error('UDP reply mismatch');
        sender.close();
        receiver.close();
        "#,
    )
    .unwrap();

    let account = ResourceAccount::default();
    let otter = otter_runtime::Otter::builder()
        .capabilities(CapabilitySet::allow_all())
        .resource_account(account.clone())
        .with_node_apis()
        .build()
        .unwrap();
    otter.blocking_run_module(&main).unwrap();

    let snapshot = account.snapshot();
    assert!(snapshot.get(ResourceClass::QueuedMessages).peak() > 0);
    assert!(snapshot.get(ResourceClass::QueuedMessageBytes).peak() > 0);
    assert_eq!(snapshot.get(ResourceClass::QueuedMessages).current(), 0);
    assert_eq!(snapshot.get(ResourceClass::QueuedMessageBytes).current(), 0);
}

#[test]
fn node_udp_receive_budget_pressure_reports_enobufs_and_recovers() {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("main.mjs");
    std::fs::write(
        &main,
        r#"
        import dgram from "node:dgram";

        const receiver = dgram.createSocket('udp4');
        await new Promise((resolve, reject) => {
          receiver.once('error', reject);
          receiver.bind(0, '127.0.0.1', resolve);
        });
        const address = receiver.address();
        const sender = dgram.createSocket('udp4');
        const failure = new Promise((resolve, reject) => {
          receiver.once('error', (error) => {
            if (error?.code === 'ENOBUFS') resolve(); else reject(error);
          });
        });
        await new Promise((resolve, reject) => {
          sender.send(Buffer.from('pressure'), address.port, '127.0.0.1', (error) => {
            if (error) reject(error); else resolve();
          });
        });
        await failure;
        sender.close();
        receiver.close();
        "#,
    )
    .unwrap();

    let account = ResourceAccount::new(
        ResourceLimits::builder()
            .limit(ResourceClass::QueuedMessageBytes, 0)
            .build(),
    );
    let otter = otter_runtime::Otter::builder()
        .capabilities(CapabilitySet::allow_all())
        .resource_account(account.clone())
        .with_node_apis()
        .build()
        .unwrap();
    otter.blocking_run_module(&main).unwrap();

    let snapshot = account.snapshot();
    assert_eq!(
        snapshot.get(ResourceClass::QueuedMessageBytes).rejections(),
        1
    );
    assert_eq!(snapshot.get(ResourceClass::QueuedMessages).current(), 0);
    assert_eq!(snapshot.get(ResourceClass::QueuedMessageBytes).current(), 0);
}

/// A `once` listener is `onceWrapper.bind(state)`, so every once-emit calls
/// `.apply` on a **bound function** receiver. The compiled method-call stub
/// resolves the method through `get_method_value_for_call` alone, which used
/// to have no bound-function branch: the resolution answered `None`, and the
/// emit died with "value is not a function" once the call site was compiled —
/// the interpreter path masked it. The hot loops below push both call sites
/// into the compiled tier before the assertions run.
#[test]
fn node_events_once_listener_survives_a_compiled_emit() {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("main.mjs");
    std::fs::write(
        &main,
        r#"
        import { EventEmitter } from "node:events";

        const e = new EventEmitter();
        e.on("warm", () => {});
        for (let i = 0; i < 20000; i++) e.emit("warm");

        let got = null;
        e.once("ping", (v) => { got = v; });
        e.emit("ping", 42);
        if (got !== 42) throw new Error("once listener saw " + got);

        const f = function () { return this.v; }.bind({ v: 7 });
        let s = 0;
        for (let i = 0; i < 20000; i++) s += f.apply(null, []);
        if (s !== 7 * 20000) throw new Error("bound apply totalled " + s);
        "#,
    )
    .unwrap();

    let mut runtime = Runtime::builder()
        .capabilities(CapabilitySet::allow_all())
        .with_node_apis()
        .build()
        .unwrap();
    runtime.run_module(&main).unwrap();
}

/// An async generator's completing resume settles the pending `next()`.
/// The awaiting signal must come from the generator's state, not from the
/// request queue: the caller's own `next()` request sits in that queue, so
/// queue-emptiness misread every completion as an await parking and left
/// the promise pending forever — `for await` over any stream never ended.
#[test]
fn async_generator_completion_settles_the_pending_next() {
    let dir = tempfile::tempdir().unwrap();
    let main = dir.path().join("main.mjs");
    std::fs::write(
        &main,
        r#"
        async function* plain() { yield 1; }
        {
            const g = plain();
            await g.next();
            const r = await g.next();
            if (r.done !== true) throw new Error("implicit end never settled");
        }
        async function* withReturn() { yield 1; return 5; }
        {
            const g = withReturn();
            await g.next();
            const r = await g.next();
            if (r.done !== true || r.value !== 5) throw new Error("return value lost");
        }
        async function* withAwait() { await Promise.resolve(); yield 1; }
        {
            const g = withAwait();
            await g.next();
            const r = await g.next();
            if (r.done !== true) throw new Error("await body never completed");
        }
        const { PassThrough } = await import("node:stream");
        {
            const c = new PassThrough();
            c.end("foobar");
            let saw = "";
            for await (const chunk of c) saw += String(chunk);
            if (saw !== "foobar") throw new Error("iteration saw " + saw);
        }
        "#,
    )
    .unwrap();

    let mut runtime = Runtime::builder()
        .capabilities(CapabilitySet::allow_all())
        .with_node_apis()
        .build()
        .unwrap();
    runtime.run_module(&main).unwrap();
}
